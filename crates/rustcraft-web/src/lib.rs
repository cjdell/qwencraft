//! RustCraft browser entry point.
//!
//! Two server backends:
//! - **built-in** (default): the game server is embedded in this wasm module
//!   and driven directly from the browser event loop;
//! - **remote**: a headless server (`rustcraft-net`) served over WebSocket —
//!   pointed at via the overlay's connect panel or `?server=ws://host:port`.
//!   The client renders server state and forwards input; it never mutates
//!   world state (the server stays authoritative in both modes).
//!
//! This crate only compiles for wasm32 (browser entry point).

#![cfg(target_arch = "wasm32")]

mod verify_gl;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    Element, HtmlCanvasElement, HtmlDivElement, HtmlElement, HtmlInputElement, KeyboardEvent,
    MessageEvent, MouseEvent, Window,
};

use rustcraft_client::Renderer;
use rustcraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use rustcraft_server::{
    Action, AgentState, Input, Key, KeySet, Server, ServerStats, Streamer, WorldUpdate,
};
use rustcraft_world::camera::view_projection;
use rustcraft_world::ChunkPos;

fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

/// Which server backs the game: the embedded one, or a headless one over
/// WebSocket. The frame loop only ever talks to this abstraction.
enum Backend {
    Builtin { server: Server, streamer: Streamer },
    Remote(RemoteLink),
}

impl Backend {
    /// This page's own player id (the renderer skips it: first person).
    fn own_id(&self) -> u32 {
        match self {
            Backend::Builtin { .. } => 0,
            Backend::Remote(r) => r.player_id,
        }
    }

    /// Apply the player's identity (name + sphere colour). Built-in: set
    /// locally; remote: send to the server, which broadcasts it to every
    /// other client via the agent list.
    fn set_profile(&mut self, name: String, color: [u8; 3]) {
        match self {
            Backend::Builtin { server, .. } => server.set_profile(0, name, color),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::Profile { name, color });
                }
            }
        }
    }

    fn set_input(&mut self, input: Input) {
        match self {
            Backend::Builtin { server, .. } => server.set_input(input),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::Input {
                        keys: input.keys.bits(),
                        dx: input.mouse_dx,
                        dy: input.mouse_dy,
                    });
                }
            }
        }
    }

    fn push_action(&mut self, a: Action) {
        match self {
            Backend::Builtin { server, .. } => server.push_action(a),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::Action(a));
                }
            }
        }
    }

    /// Advance the simulation. The remote backend is a no-op: its world
    /// ticks on the server at a fixed rate, independent of this page.
    fn tick(&mut self, dt: f64) {
        if let Backend::Builtin { server, .. } = self {
            server.tick(dt);
        }
    }

    fn take_world_updates(&mut self) -> Vec<WorldUpdate> {
        match self {
            Backend::Builtin { server, streamer } => {
                // This page is the single viewer of its built-in world: drain
                // edits, resend the changed chunks it holds, and stream the
                // world around the player.
                let dirty = server.drain_dirty();
                streamer.apply_edits(server.world(), &dirty);
                let vp = server.player_state().pos;
                streamer.tick(server.world_mut(), vp);
                streamer.take()
            }
            Backend::Remote(r) => std::mem::take(&mut r.inbound),
        }
    }

    fn player_state(&self) -> AgentState {
        match self {
            Backend::Builtin { server, .. } => server.player_state(),
            Backend::Remote(r) => r.player.clone(),
        }
    }

    fn agents(&self) -> Vec<AgentState> {
        match self {
            Backend::Builtin { server, .. } => server.agents(),
            Backend::Remote(r) => r.agents.clone(),
        }
    }

    fn stats(&self) -> ServerStats {
        match self {
            Backend::Builtin { server, streamer } => server.stats(streamer.sent_count()),
            Backend::Remote(r) => r.stats,
        }
    }

    fn npc_load_config(&self) -> (u32, f32) {
        match self {
            Backend::Builtin { server, .. } => server.npc_load_config(),
            Backend::Remote(r) => r.npc_load,
        }
    }

    /// Report chunks the terrain pool evicted (compaction). The streamer
    /// forgets them and its normal stream re-sends the ones that are
    /// visible again — nearest-first, with lookahead, at the stream rate.
    fn report_evicted(&mut self, evicted: Vec<ChunkPos>) {
        if evicted.is_empty() {
            return;
        }
        match self {
            Backend::Builtin { server: _, streamer } => streamer.note_evicted(&evicted),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::Evicted(evicted));
                }
            }
        }
    }
}

/// A live connection to a headless server. Inbound messages are applied by
/// the WebSocket `onmessage` handler (single-threaded wasm: it runs between
/// frame-loop turns, so no synchronization is needed); the frame loop then
/// consumes the results.
struct RemoteLink {
    /// Monotonic id; handlers of a replaced link are no-ops for it.
    id: u32,
    ws: web_sys::WebSocket,
    url: String,
    /// True once the server's Hello has arrived (input flows from then on).
    connected: bool,
    /// This page's player id in the shared world (from Hello); u32::MAX
    /// until it arrives.
    player_id: u32,
    seed: Option<u64>,
    /// Chunk updates received, waiting for the frame loop.
    inbound: Vec<WorldUpdate>,
    /// Latest server state (the rendering source of truth).
    player: AgentState,
    agents: Vec<AgentState>,
    stats: ServerStats,
    npc_load: (u32, f32),
}

impl RemoteLink {
    /// Encode and fire one client message (best-effort; the browser queues
    /// until the socket opens and we drop sends before Hello anyway).
    fn send(&mut self, msg: ClientMsg) {
        let data = msg.encode();
        let _ = self.ws.send_with_u8_array(&data);
    }
}

struct App {
    backend: Backend,
    /// Seed of the built-in world (used when switching back to it).
    builtin_seed: u64,
    renderer: Option<Renderer>,
    hud: HtmlDivElement,
    overlay: HtmlDivElement,
    keys: KeySet,
    mouse_dx: f32,
    mouse_dy: f32,
    actions: Vec<Action>,
    /// Camera aim (yaw/pitch) of the last rendered frame. Click actions are
    /// stamped with this so the server raycasts with the aim the player
    /// actually saw — mouse deltas that land after a click must not move
    /// the targeted block.
    aim_yaw: f32,
    aim_pitch: f32,
    locked: bool,
    last_time: f64,
    frames: u32,
    fps_time: f64,
    fps: f32,
    // Last frame timestamp (ms) — guards against double-driving from both
    // the rAF loop and the setInterval fallback.
    last_frame_ms: f64,
    // Verify mode (?verify=1): accumulate the streamed chunk regions and
    // re-render them through a WebGL2 "shadow" renderer whose pixels can be
    // read back even in headless browsers (see verify_gl.rs).
    verify_mode: bool,
    /// Headless stress test: hold W and walk (turning away when blocked by
    /// terrain), exercising terrain streaming + pool compaction.
    walk_mode: bool,
    /// `?taglog=1`: log name-tag screen positions every 5 s (headless
    /// verification that tags track their players).
    tag_log: bool,
    /// Wall-clock ms of the last TAGS telemetry line.
    last_tag_log_at: f64,
    // Walk-mode steering state: if less than 1 block of horizontal progress
    // is made per 1s window (jitter against a slope/wall counts as stuck),
    // turn 90° — always the same way, so a full circle is covered within 4
    // episodes (the player can then find an exit from e.g. a 1-block ditch).
    walk_anchor: [f32; 2],
    /// Walk test fly phase (starts at t=30s): hold W+Space and ramp the
    /// fly speed to the max, exercising fly mode + high-speed streaming.
    walk_fly: bool,
    /// Walk test return leg (starts at t=36s): 180° turn, fly back over
    /// the just-flown route (re-enters pool-evicted terrain).
    walk_return: bool,
    /// Walk test: fly off (event-driven, when the return flight is back
    /// near the walk endpoint) — the player then walks through the
    /// re-entered (evicted) terrain slowly, so the final view is that
    /// terrain and the POOL hole count is meaningful (drained, not fresh
    /// territory).
    walk_flyoff: bool,
    /// Walk test: the xz where the fly phase started (the walk endpoint)
    /// — the return flight lands when it is back within 64 blocks of this.
    walk_end_pos: [f32; 2],
    /// Previous frame's player xz (event-driven walk-test transitions;
    /// the input section runs before this frame's player state exists).
    last_player_xz: [f32; 2],
    /// Previous frame's xz (for accumulating distance walked).
    walk_anchor_prev: [f32; 2],
    /// Total horizontal distance walked (for the WALK telemetry).
    walk_dist: f64,
    /// Number of stuck episodes so far (episode 1 hops at the current wall,
    /// episodes 2-4 add a 90° turn each — a full circle is covered).
    walk_episodes: u32,
    pending_walk_turn: f32,
    /// Set with the turn: a hop (with W held) clears 1-block steps that
    /// would otherwise trap the walker (the engine has no auto-step).
    pending_walk_jump: bool,
    frames_total: u32,
    verify_done: bool,
    // Per-phase frame timings (ms), accumulated since the last HUD update.
    perf_tick_ms: f64,
    perf_mesh_ms: f64,
    perf_render_ms: f64,
    hud_updates: u32,
    verify_regions: HashMap<ChunkPos, Vec<u8>>,
    gl_verify: Option<verify_gl::GlVerifier>,

    // Remote-server bookkeeping (the live link itself lives in `backend`).
    next_link_id: u32,
    /// NPC load armed via `?npcs=`, applied once a remote connection says
    /// Hello (the built-in backend already got it in `App::new`).
    pending_npcs: Option<(u32, f32)>,
    /// Status line of the overlay's server-connect panel.
    server_status: HtmlDivElement,
    // ---- Options panel (player identity + server connection) -----------
    /// The player's display name (sanitised fallback "Player").
    player_name: String,
    /// The player's sphere colour (options palette).
    player_color: [u8; 3],
    /// Container for other players' name tags (one div per remote player).
    tags: HtmlDivElement,
    /// Live name-tag elements by agent id (created lazily, removed when the
    /// player leaves).
    tag_els: std::collections::HashMap<u32, HtmlDivElement>,
    // Keep event closures alive for the life of the page.
    _closures: Vec<*mut std::ffi::c_void>,
}

impl App {
        fn new(
        seed: u64,
        hud: HtmlDivElement,
        overlay: HtmlDivElement,
        server_status: HtmlDivElement,
        tags: HtmlDivElement,
        verify_mode: bool,
        walk_mode: bool,
        tag_log: bool,
        npcs: Option<(u32, f32)>,
    ) -> Self {
        let mut server = Server::new(seed);
        let spawn = server.player_state().pos;
        let streamer = Streamer::new();
        let mut actions = Vec::new();
        if let Some((count, spacing)) = npcs {
            server.set_npc_load(count, spacing);
            // Applied on the first tick: spawn the load-test cloud.
            actions.push(Action::NpcLoad);
        }
        App {
            backend: Backend::Builtin { server, streamer },
            builtin_seed: seed,
            walk_mode,
            tag_log,
            last_tag_log_at: 0.0,
            walk_anchor: [0.0; 2],
            walk_fly: false,
            walk_return: false,
            walk_flyoff: false,
            walk_end_pos: [spawn.x, spawn.z],
            last_player_xz: [spawn.x, spawn.z],
            walk_anchor_prev: [spawn.x, spawn.z],
            walk_dist: 0.0,
            walk_episodes: 0,
            pending_walk_turn: 0.0,
            pending_walk_jump: false,
            renderer: None,
            hud,
            overlay,
            keys: KeySet::default(),
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            actions,
            aim_yaw: 0.0,
            aim_pitch: 0.0,
            locked: false,
            last_time: 0.0,
            frames: 0,
            fps_time: 0.0,
            fps: 0.0,
            last_frame_ms: 0.0,
            verify_mode,
            frames_total: 0,
            verify_done: false,
            perf_tick_ms: 0.0,
            perf_mesh_ms: 0.0,
            perf_render_ms: 0.0,
            hud_updates: 0,
            verify_regions: HashMap::new(),
            gl_verify: None,
            next_link_id: 0,
            pending_npcs: npcs,
            server_status,
            player_name: "Player".to_string(),
            player_color: [255, 255, 255],
            tags,
            tag_els: std::collections::HashMap::new(),
            _closures: Vec::new(),
        }
    }

    /// This page's own player id (the renderer skips it: first person).
    fn own_id(&self) -> u32 {
        self.backend.own_id()
    }

    /// Send the current identity (name + colour) to the active backend.
    fn apply_profile(&mut self) {
        self.backend
            .set_profile(self.player_name.clone(), self.player_color);
    }

    /// Create (once per remote player) a name-tag element in the container.
    fn make_tag(&mut self) -> HtmlDivElement {
        let doc = web_sys::window().expect("window").document().expect("document");
        let el: HtmlDivElement = doc
            .create_element("div")
            .expect("div")
            .dyn_into()
            .expect("div");
        el.set_class_name("tag");
        el.style().set_property("display", "none").ok();
        self.tags.append_child(&el).expect("append tag");
        el
    }

    /// Position (or hide) the other players' name tags above their spheres.
    /// Only players get tags (NPCs stay plain spheres); own player is the
    /// camera and is skipped. The projection mirrors the renderer's exactly
    /// (same fov/near/far), so tags sit where the spheres are drawn.
    fn update_name_tags(
        &mut self,
        agents: &[AgentState],
        cam: [f32; 3],
        yaw: f32,
        pitch: f32,
        w: u32,
        h: u32,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let own_id = self.own_id();
        let aspect = w as f32 / h as f32;
        // Same parameters as the renderer's view-projection (fov 1.15,
        // near 0.1, far 300).
        let vp = view_projection(cam, yaw, pitch, aspect, 1.15, 0.1, 300.0);
        let mut seen = std::collections::HashSet::new();
        for s in agents.iter().filter(|s| s.is_player && s.id != own_id) {
            seen.insert(s.id);
            let label = if s.name.is_empty() {
                format!("P{}", s.id)
            } else {
                s.name.clone()
            };
            let el = match self.tag_els.get(&s.id) {
                Some(e) => e.clone(),
                None => {
                    let e = self.make_tag();
                    // Rare event (another player appeared): log it so
                    // headless runs can assert players are being tagged.
                    log(&format!("RustCraft: name tag created for player {} (\"{}\")", s.id, label));
                    self.tag_els.insert(s.id, e.clone());
                    e
                }
            };
            if el.text_content().unwrap_or_default() != label {
                el.set_text_content(Some(&label));
            }
            // Project the point just above the sphere's top.
            let top = [s.pos.x, s.pos.y + 2.0 * s.radius + 0.35, s.pos.z];
            let Some((sx, sy)) = Self::project_to_screen(&vp, top, w, h) else {
                el.style().set_property("display", "none").ok();
                continue;
            };
            el.style().set_property("display", "block").ok();
            el.style().set_property("left", &format!("{sx:.1}px")).ok();
            el.style().set_property("top", &format!("{sy:.1}px")).ok();
        }
        // Drop tags of players that left the shared world.
        let stale: Vec<u32> = self
            .tag_els
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        for id in stale {
            if let Some(el) = self.tag_els.remove(&id) {
                el.remove();
            }
        }
        // Telemetry (?taglog=1): sample visible tag positions on a wall-clock
        // cadence — frame counts differ wildly between virtual time (~60 fps)
        // and a slow SwiftShader machine (a few fps), so time is the common
        // denominator.
        if self.tag_log {
            let now = js_sys::Date::now();
            if now - self.last_tag_log_at > 5000.0 {
                self.last_tag_log_at = now;
                let mut parts = Vec::new();
                for (id, el) in &self.tag_els {
                    let display = el.style().get_property_value("display").unwrap_or_default();
                    if display == "none" {
                        continue;
                    }
                    let l = el.style().get_property_value("left").unwrap_or_default();
                    let t = el.style().get_property_value("top").unwrap_or_default();
                    parts.push(format!("{id}:{l}/{t}"));
                }
                if !parts.is_empty() {
                    log(&format!("TAGS {}", parts.join(" ")));
                }
            }
        }
    }

    /// World point → CSS pixels (None when behind the camera or far off
    /// screen). Delegates to the pure [`project_point`].
    fn project_to_screen(vp: &[f32; 16], p: [f32; 3], w: u32, h: u32) -> Option<(f32, f32)> {
        project_point(vp, p, w as f32, h as f32)
    }

    /// The live remote link's id (u32::MAX when not in remote mode).
    fn remote_id(&self) -> u32 {
        match &self.backend {
            Backend::Remote(r) => r.id,
            Backend::Builtin { .. } => u32::MAX,
        }
    }

    /// Update the connect panel's status line (and the console log).
    fn set_server_status(&mut self, msg: &str) {
        self.server_status.set_text_content(Some(msg));
        log(&format!("RustCraft: server: {msg}"));
    }

    /// Drop any remote link and return to a fresh built-in server.
    fn fallback_to_builtin(&mut self) {
        self.backend = Backend::Builtin {
            server: Server::new(self.builtin_seed),
            streamer: Streamer::new(),
        };
        // The previous world's terrain belongs to the old backend.
        if let Some(r) = self.renderer.as_mut() {
            r.clear_terrain();
        }
        self.keys = KeySet::default();
    }

    /// Apply decoded server messages to the remote link `id`.
    fn apply_remote_messages(&mut self, id: u32, msgs: Vec<ServerMsg>, url: &str) {
        let mut hello_seed: Option<u64> = None;
        if let Backend::Remote(r) = &mut self.backend {
            if r.id != id {
                return;
            }
            for m in msgs {
                match m {
                    ServerMsg::Hello { version, seed, player_id } => {
                        if version != PROTOCOL_VERSION {
                            log(&format!(
                                "RustCraft: server speaks protocol {version}, client has {PROTOCOL_VERSION} — closing"
                            ));
                            let _ = r.ws.close();
                            return; // the close handler does the fallback
                        }
                        r.connected = true;
                        r.seed = Some(seed);
                        r.player_id = player_id;
                        hello_seed = Some(seed);
                        // Announce our identity right away: the shared
                        // world broadcasts it so other clients can render
                        // us (sphere + name tag) from the first tick.
                        r.send(ClientMsg::Profile {
                            name: self.player_name.clone(),
                            color: self.player_color,
                        });
                        log(&format!(
                            "RustCraft: remote server connected (seed {seed}, player {player_id})"
                        ));
                    }
                    ServerMsg::PlayerState(s) => r.player = s,
                    ServerMsg::Agents(v) => r.agents = v,
                    ServerMsg::Chunk { pos, data } => {
                        r.inbound.push(WorldUpdate::Chunk { pos, data })
                    }
                    ServerMsg::Stats(s) => r.stats = s,
                    ServerMsg::NpcLoad { count, spacing } => r.npc_load = (count, spacing),
                }
            }
        } else {
            return;
        }
        if let Some(seed) = hello_seed {
            self.set_server_status(&format!("connected: {url} (seed {seed})"));
            // ?npcs= armed before connecting: apply it now that the world
            // is live.
            if let Some((count, spacing)) = self.pending_npcs.take() {
                if let Backend::Remote(r) = &mut self.backend {
                    r.send(ClientMsg::SetNpcLoad { count, spacing });
                }
            }
        }
    }


    /// Run the WebGL2 shadow render and log `VERIFY_PIXELS r,g,b;...`,
    /// streaming the full frame once as base64 `VERIFY_PNG` chunks so
    /// verify.sh can reconstruct a real screenshot of the 3D scene (the
    /// WebGPU canvas itself cannot be composited in headless Chromium).
    fn run_gl_verify(&mut self) {
        let p = self.backend.player_state();
        let cam = [p.pos.x, p.pos.y + rustcraft_server::agent::EYE_HEIGHT, p.pos.z];
        if self.gl_verify.is_none() {
            let doc = web_sys::window().and_then(|w| w.document());
            self.gl_verify = doc.as_ref().and_then(|d| verify_gl::GlVerifier::new(d));
        }
        let Some(gl) = self.gl_verify.as_ref() else {
            log("VERIFY_PIXELS gl context unavailable");
            return;
        };
        let highlight = p.target.map(|t| (t.x, t.y, t.z));
        match gl.readback(&self.verify_regions, cam, p.yaw, p.pitch, highlight) {
            Some(grid) => log(&format!("VERIFY_PIXELS {grid}")),
            None => log("VERIFY_PIXELS readback failed"),
        }
        if !self.verify_done {
            match gl.framebuffer() {
                Some(fb) => {
                    let b64 = verify_gl::base64(&fb);
                    const CHUNK: usize = 12_000;
                    let n_chunks = (b64.len() + CHUNK - 1) / CHUNK;
                    for (i, part) in b64.as_bytes().chunks(CHUNK).enumerate() {
                        let s = std::str::from_utf8(part).unwrap();
                        log(&format!("VERIFY_PNG {i}/{n_chunks} {s}"));
                    }
                    self.verify_done = true;
                }
                None => log("VERIFY_PNG framebuffer read failed"),
            }
        }
    }

    /// Advance the simulation by one frame and render.
    ///
    /// Driven by requestAnimationFrame in real browsers, and by a 16 ms
    /// setInterval fallback in headless environments where rAF never fires.
    /// The 8 ms guard prevents double rendering when both are active.
    fn frame(&mut self) {
        let now_ms = js_sys::Date::now();
        if now_ms - self.last_frame_ms < 8.0 {
            return;
        }
        self.last_frame_ms = now_ms;

        let now = now_ms as f64 / 1000.0;
        let dt = if self.last_time == 0.0 {
            0.0
        } else {
            (now - self.last_time).min(0.25)
        };
        self.last_time = now;

        // Input -> server.
        let mut input = Input::default();
        input.keys = self.keys;
        input.mouse_dx = self.mouse_dx;
        input.mouse_dy = self.mouse_dy;
        self.mouse_dx = 0.0;
        self.mouse_dy = 0.0;
        if self.walk_mode {
            // Hold W: a walk through fresh terrain (turning away when
            // blocked). The trail of streamed chunks behind the player is
            // the worst case for the terrain pool. From t=30s: a horizontal
            // fly phase (W at the max speed, level pitch — no climb: the
            // long corridor is what fills the pool and evicts the walk
            // endpoint as fog-bound trail). At t=38s: turn 180° and fly
            // straight back along the route; back near the walk endpoint
            // the player lands and walks through the re-entered terrain.
            // The re-entry exercises the eviction->re-stream path: without
            // it, the walked-back terrain would stay a hole, and the POOL
            // missing count (the player ends the run walking through that
            // re-entered terrain) would show it.
            input.keys.insert(Key::W);
            if self.walk_fly {
                self.actions.push(Action::FlyFaster); // clamps at the max
            }
            // Fly start: level the pitch so the flight is horizontal
            // (fly moves along the look direction; a climb would leave the
            // return flight landing far from the route).
            if self.frames_total == 1800 {
                input.mouse_dy += self.aim_pitch / 0.0024;
            }
            if self.frames_total == 2280 && !self.walk_return {
                self.walk_return = true;
                input.mouse_dx += std::f32::consts::PI / 0.0024; // 180°
                log("WALK t=38s — turn around, fly back to the route");
            }
            if self.walk_return && !self.walk_flyoff {
                let dx = self.last_player_xz[0] - self.walk_end_pos[0];
                let dz = self.last_player_xz[1] - self.walk_end_pos[1];
                let near = (dx * dx + dz * dz).sqrt() < 64.0;
                if near || self.frames_total == 3300 { // fallback: t=55s
                    self.walk_flyoff = true;
                    self.actions.push(Action::ToggleFly); // land: walk back
                    log(&format!(
                        "WALK t={}s — fly off at ({:.0},{:.0}) (walk back through re-entered terrain)",
                        self.frames_total / 60,
                        self.last_player_xz[0],
                        self.last_player_xz[1]
                    ));
                }
            }
            if self.pending_walk_jump {
                input.keys.insert(Key::Space);
                self.pending_walk_jump = false;
            }
            input.mouse_dx += self.pending_walk_turn;
            self.pending_walk_turn = 0.0;
        }
        self.backend.set_input(input);
        for a in self.actions.drain(..) {
            self.backend.push_action(a);
        }
        let t_tick = js_sys::Date::now();
        self.backend.tick(dt);
        let t_mesh = js_sys::Date::now();

        let updates = self.backend.take_world_updates();
        if self.verify_mode {
            for u in &updates {
                match u {
                    WorldUpdate::Chunk { pos, data } => {
                        self.verify_regions.insert(*pos, data.clone());
                    }
                }
            }
        }
        let agents = self.backend.agents();
        let player = self.backend.player_state();
        // The rendered camera uses exactly this state; stamp it for click
        // actions (see the `aim_*` field docs).
        self.aim_yaw = player.yaw;
        self.aim_pitch = player.pitch;
        self.last_player_xz = [player.pos.x, player.pos.z];
        if self.walk_mode {
            // At t=30s switch to the fly phase (max-speed straight flight).
            if self.frames_total == 1800 && !self.walk_fly {
                self.walk_fly = true;
                // The walk endpoint: the return flight lands here.
                self.walk_end_pos = [player.pos.x, player.pos.z];
                self.actions.push(Action::ToggleFly);
                log("WALK t=30s — fly phase on");
            }
            // Steer around obstacles: over each 1s window, less than 1 block
            // of horizontal progress means stuck (jitter against a slope or
            // wall) — hop, and turn 90° the same way each episode.
            if self.frames_total % 60 == 59 {
                let d = (player.pos.x - self.walk_anchor[0]).abs()
                    + (player.pos.z - self.walk_anchor[1]).abs();
                if d < 1.0 {
                    // Hop to clear any 1-block step in the way; after the
                    // first attempt (at the wall currently faced) also turn
                    // 90°, same direction every time (see field docs).
                    self.walk_episodes += 1;
                    self.pending_walk_jump = true;
                    if self.walk_episodes % 4 != 1 {
                        self.pending_walk_turn = 1.5708 / 0.0024;
                    }
                    log(&format!(
                        "WALK t={:.0}s stuck at ({:.0},{:.0}) — hop (episode {})",
                        self.frames_total as f64 / 60.0,
                        player.pos.x,
                        player.pos.z,
                        self.walk_episodes
                    ));
                }
                self.walk_anchor = [player.pos.x, player.pos.z];
            }
            // Accumulate distance walked (path length, not displacement —
            // the walker may turn around).
            let dx = player.pos.x - self.walk_anchor_prev[0];
            let dz = player.pos.z - self.walk_anchor_prev[1];
            self.walk_dist += (dx * dx + dz * dz).sqrt() as f64;
            self.walk_anchor_prev = [player.pos.x, player.pos.z];
            if self.frames_total % 300 == 0 {
                log(&format!(
                    "WALK t={:.0}s pos={:.0},{:.0},{:.0} dist={:.0} fly={} speed={:.0} on_ground={}",
                    self.frames_total as f64 / 60.0,
                    player.pos.x,
                    player.pos.y,
                    player.pos.z,
                    self.walk_dist,
                    u8::from(player.fly),
                    player.fly_speed,
                    player.on_ground
                ));
            }
        }

        // The rendered camera (eye position); shared by the name-tag
        // projection and the render pass.
        let cam = [
            player.pos.x,
            player.pos.y + rustcraft_server::agent::EYE_HEIGHT,
            player.pos.z,
        ];
        // Other players' name tags (DOM overlay, projected with the same
        // view-projection as the render). Done before `agents` moves into
        // the renderer; own player is skipped (first person). The
        // projection must land in **CSS** pixels (DOM layout units) — the
        // drawing buffer is devicePixelRatio× larger on high-DPI displays.
        let (w, h) = self
            .renderer
            .as_ref()
            .map(|r| r.css_size())
            .unwrap_or((0, 0));
        let own_id = self.own_id();
        self.update_name_tags(&agents, cam, player.yaw, player.pitch, w, h);

        if let Some(r) = &mut self.renderer {
            r.apply_updates(updates);
            // Report every chunk the pool evicted (visible or fog-bound).
            // The streamer forgets them and re-sends the ones that are
            // visible again (its normal stream: nearest-first, with
            // lookahead). Without this, chunks evicted while far away —
            // fully fogged, dropped silently — would stay holes when the
            // player walks back over the terrain.
            let evicted = r.take_evicted();
            if !evicted.is_empty() {
                self.backend.report_evicted(evicted);
            }
            let t_render = js_sys::Date::now();
            // All agents except our own (rendered first person): other
            // players are spheres like NPCs.
            r.set_agents(agents, own_id);
            r.set_highlight(player.target.map(|t| [t.x, t.y, t.z]));
            r.render(cam, player.yaw, player.pitch);
            let t_done = js_sys::Date::now();
            self.perf_tick_ms += t_mesh - t_tick;
            self.perf_mesh_ms += t_render - t_mesh;
            self.perf_render_ms += t_done - t_render;
            if r.take_first_frame() {
                log("RustCraft: first frame rendered");
            }
            self.frames_total += 1;
            if self.verify_mode && self.frames_total == 410 && !self.verify_done {
                // Plenty of chunks have been streamed and meshed by now;
                // run the WebGL2 shadow readback + one-shot screenshot.
                self.run_gl_verify();
            }
        } else {
            drop(updates);
            drop(agents);
        }

        // FPS + HUD.
        self.frames += 1;
        if self.fps_time == 0.0 {
            self.fps_time = now;
        }
        if now - self.fps_time >= 0.5 {
            self.fps = self.frames as f32 / (now - self.fps_time) as f32;
            let n = self.frames.max(1) as f64;
            let (pt, pm, pr) = (
                self.perf_tick_ms / n,
                self.perf_mesh_ms / n,
                self.perf_render_ms / n,
            );
            self.perf_tick_ms = 0.0;
            self.perf_mesh_ms = 0.0;
            self.perf_render_ms = 0.0;
            self.frames = 0;
            self.fps_time = now;
            self.hud_updates += 1;
            let stats = self.backend.stats();
            let fly = if player.fly {
                format!(" | FLY {:.0} b/s [F off · Q/E speed]", player.fly_speed)
            } else {
                String::new()
            };
            let (load_count, load_spacing) = self.backend.npc_load_config();
            // NPC load-test line: the configured load, the live count, and
            // the local-block-window stats (reset when the load changes):
            // hit % = lookups served by the per-agent window instead of the
            // world's chunk buffers; solid-fb = solid reads that still fell
            // back to the buffers (should stay ~0 in steady state).
            let cache = stats.cache;
            let hit_pct = if cache.lookups > 0 {
                100.0 * cache.hits as f64 / cache.lookups as f64
            } else {
                100.0
            };
            let npc_line = format!(
                "\nnpc {load_count}/{load_spacing:.0}m [N load · C clear · I/U count · [ ] spacing] (live {}) | window {hit_pct:.1}% · solid-fb {} · rebuilds {}",
                stats.npcs, cache.solid_misses, cache.rebuilds
            );
            // Which backend is serving the world (built-in vs. the remote
            // server the user connected to; "connecting" while the socket
            // is still handshaking).
            let net = match &self.backend {
                Backend::Builtin { .. } => format!("builtin (seed {})", self.builtin_seed),
                Backend::Remote(r) => {
                    if r.connected {
                        format!("{} (seed {})", r.url, r.seed.unwrap_or(0))
                    } else {
                        format!("connecting {}…", r.url)
                    }
                }
            };
            self.hud.set_inner_html(&format!(
                "fps {:.0} | perf tick={:.1} mesh={:.1} draw={:.1} ms/f | pos {:.0} {:.0} {:.0} | chunks {} sent / {} gen | edits {} | agents {} | net {}{}{}",
                self.fps,
                pt,
                pm,
                pr,
                player.pos.x,
                player.pos.y,
                player.pos.z,
                stats.chunks_sent,
                stats.chunks_generated,
                stats.deltas,
                stats.agents,
                net,
                fly,
                npc_line
            ));
            if self.hud_updates % 20 == 0 {
                let nf = n as u32;
                log(&format!(
                    "PERF fps={:.0} tick={:.1} mesh={:.1} render={:.1} ms/f (frames={nf})",
                    self.fps, pt, pm, pr
                ));
                // Terrain pool state: chunks held, plus meshed-but-evicted
                // chunks that are clearly visible (holes). `missing` counts
                // chunks the client has rendered geometry for that are no
                // longer in the pool — a sustained non-zero count means the
                // pool is losing visible chunks (capacity too small, or the
                // eviction->re-stream path is broken). The walk test asserts
                // on this.
                if let Some(r) = &self.renderer {
                    let missing = r.missing_visible(6).len();
                    log(&format!(
                        "POOL chunks={} missing={} agents={}",
                        r.chunk_count(),
                        missing,
                        stats.agents
                    ));
                }
            }
        }
    }
}

/// Connect to a headless server at `url` (see `normalize_ws_url`).
///
/// The new backend takes over immediately (the old link is closed; its
/// handlers stay alive but are no-ops via the link id check). Input and
/// actions are held until the server's Hello arrives; a failed or dropped
/// connection falls back to the built-in server so the app stays playable.
fn connect_remote(app: &Rc<RefCell<App>>, raw_url: &str) {
    let Some(url) = normalize_ws_url(raw_url) else {
        app.borrow_mut()
            .set_server_status(&format!("invalid URL: {raw_url:?} — use ws://host:port"));
        return;
    };
    // Close any previous link first.
    if let Backend::Remote(r) = &app.borrow().backend {
        let _ = r.ws.close();
    }
    let Ok(ws) = web_sys::WebSocket::new(&url) else {
        app.borrow_mut()
            .set_server_status(&format!("invalid URL: {raw_url:?} — use ws://host:port"));
        return;
    };
    // Some browser builds default the binary type to "blob"; the codec
    // wants raw ArrayBuffer bytes.
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
    let id = {
        let mut a = app.borrow_mut();
        let id = a.next_link_id;
        a.next_link_id += 1;
        a.backend = Backend::Remote(RemoteLink {
            id,
            ws: ws.clone(),
            url: url.clone(),
            connected: false,
            player_id: u32::MAX,
            seed: None,
            inbound: Vec::new(),
            player: AgentState::default(),
            agents: Vec::new(),
            stats: ServerStats::default(),
            npc_load: (
                rustcraft_server::NPC_COUNT_DEFAULT,
                rustcraft_server::NPC_SPACING_DEFAULT,
            ),
        });
        // The old world's terrain belongs to the old backend.
        if let Some(r) = a.renderer.as_mut() {
            r.clear_terrain();
        }
        a.set_server_status(&format!("connecting to {url} …"));
        id
    };
    log(&format!("RustCraft: connecting to {url}"));

    // open
    {
        let app_cb = app.clone();
        let url_cb = url.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            if app_cb.borrow().remote_id() != id {
                return;
            }
            log(&format!("RustCraft: remote socket open: {url_cb}"));
        });
        ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);
    }
    // message
    {
        let app_cb = app.clone();
        let url_cb = url.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            // Binary frames arrive as ArrayBuffers; view them as bytes.
            let bytes = js_sys::Uint8Array::new(&e.data()).to_vec();
            let (msgs, _) = ServerMsg::decode_stream(&bytes);
            if msgs.is_empty() {
                return;
            }
            app_cb.borrow_mut().apply_remote_messages(id, msgs, &url_cb);
        });
        ws.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);
    }
    // close
    {
        let app_cb = app.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let mut a = app_cb.borrow_mut();
            if a.remote_id() != id {
                return;
            }
            let (had_hello, url) = match &a.backend {
                Backend::Remote(r) => (r.connected, r.url.clone()),
                _ => return,
            };
            a.fallback_to_builtin();
            if had_hello {
                log(&format!(
                    "RustCraft: remote server {url} disconnected — running built-in server"
                ));
                a.set_server_status(
                    "disconnected — running built-in server (re-click Connect to retry)",
                );
            } else {
                log(&format!(
                    "RustCraft: remote connection to {url} failed — running built-in server"
                ));
                a.set_server_status(
                    "connection failed — running built-in server (re-click Connect to retry)",
                );
            }
        });
        ws.set_onclose(Some(cb.as_ref().unchecked_ref()));
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);
    }
    // error (the close event follows and performs the fallback)
    {
        let cb = Closure::<dyn FnMut()>::new(|| {
            log("RustCraft: remote socket error");
        });
        ws.set_onerror(Some(cb.as_ref().unchecked_ref()));
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);
    }
}

fn key_from_code(code: &str) -> Option<Key> {
    match code {
        "KeyW" => Some(Key::W),
        "KeyA" => Some(Key::A),
        "KeyS" => Some(Key::S),
        "KeyD" => Some(Key::D),
        "Space" => Some(Key::Space),
        "ShiftLeft" => Some(Key::ShiftLeft),
        "Digit0" => Some(Key::Key0),
        "KeyF" => Some(Key::F),
        "KeyE" => Some(Key::E),
        "KeyQ" => Some(Key::Q),
        "KeyN" => Some(Key::KeyN),
        "KeyC" => Some(Key::KeyC),
        "KeyI" => Some(Key::KeyI),
        "KeyU" => Some(Key::KeyU),
        "BracketLeft" => Some(Key::BracketLeft),
        "BracketRight" => Some(Key::BracketRight),
        _ => None,
    }
}

fn params_from_url() -> (u64, bool, bool, bool, Option<(u32, f32)>, Option<String>) {
    let mut seed = 1337u64;
    let mut verify = false;
    let mut walk = false;
    // `taglog=1` makes the app log name-tag positions every 5 s (headless
    // verification that tags track their players).
    let mut tag_log = false;
    // `npcs=COUNT[:SPACING]` starts the app with an NPC load already
    // spawned (headless load testing without a keyboard).
    let mut npcs: Option<(u32, f32)> = None;
    // `server=ws://host:port` points the app at a headless server
    // (pre-fills the connect panel and auto-connects; headless testing).
    let mut server: Option<String> = None;
    if let Some(win) = web_sys::window() {
        let search = win.location().search().unwrap_or_default();
        for part in search.split('?').flat_map(|s| s.split('&')) {
            if let Some(v) = part.strip_prefix("seed=") {
                if let Ok(n) = v.split('#').next().unwrap_or("").parse::<u64>() {
                    seed = n;
                }
            } else if part.strip_prefix("verify=").is_some_and(|v| v != "0") {
                verify = true;
            } else if part.strip_prefix("walk=").is_some_and(|v| v != "0") {
                walk = true;
            } else if part.strip_prefix("taglog=").is_some_and(|v| v != "0") {
                tag_log = true;
            } else if let Some(v) = part.strip_prefix("npcs=") {
                if !v.is_empty() && v != "0" {
                    let (cs, ss) = match v.split_once(':') {
                        Some((c, s)) => (c, s),
                        None => (v, ""),
                    };
                    let count = cs.parse::<u32>().ok();
                    let spacing = if ss.is_empty() {
                        None
                    } else {
                        ss.parse::<f32>().ok()
                    };
                    if let (Some(c), s) = (count, spacing) {
                        npcs = Some((c, s.unwrap_or(rustcraft_server::NPC_SPACING_DEFAULT)));
                    }
                }
            } else if let Some(v) = part.strip_prefix("server=") {
                if !v.is_empty() {
                    server = Some(v.to_string());
                }
            }
        }
    }
    (seed, verify, walk, tag_log, npcs, server)
}

/// Accept `ws://…`, `wss://…`, or bare `host:port` (ws:// implied).
/// Returns None for empty input or foreign schemes. The server speaks
/// WebSocket at `/ws` on its single port, so a bare `host[:port]` (or
/// `host[:port]/`) is upgraded to `…/ws`.
fn normalize_ws_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let with_scheme = if s.starts_with("ws://") || s.starts_with("wss://") {
        s.to_string()
    } else if s.contains("://") {
        return None;
    } else {
        format!("ws://{s}")
    };
    let (scheme_host, rest) = with_scheme.split_once("://")?;
    // Everything before the first '/', '?' or '#' is the authority.
    let cut = rest
        .as_bytes()
        .iter()
        .position(|&b| b == b'/' || b == b'?' || b == b'#')
        .unwrap_or(rest.len());
    let host = &rest[..cut];
    if host.is_empty() {
        return None;
    }
    let tail = &rest[cut..];
    // Bare host[:port] (or host[:port]/) means the /ws endpoint.
    if tail.is_empty() || tail == "/" {
        return Some(format!("{scheme_host}://{host}/ws"));
    }
    Some(format!("{scheme_host}://{host}{tail}"))
}

// World point → screen pixels for the name tags. Lives in
// `rustcraft_world::camera` (next to `view_projection`) so the exact math
// the tags use is host-tested there (this crate is wasm-only).
pub use rustcraft_world::camera::project_point;

/// True when the keyboard event's target is a text field (the options
/// panel's name/server-URL inputs). Game input must ignore such events
/// entirely (see the keydown/keyup guards).
fn event_target_is_text_input(e: &KeyboardEvent) -> bool {
    e.target()
        .and_then(|t| t.dyn_ref::<HtmlElement>().cloned())
        .is_some_and(|t| matches!(t.tag_name().as_str(), "INPUT" | "TEXTAREA"))
}

/// True when `target` is `ancestor` or nested inside it (the options panel
/// is the pointer-lock exclusion zone).
fn is_inside(target: &Element, ancestor: &Element) -> bool {
    let mut cur: Option<Element> = Some(target.clone());
    while let Some(el) = cur {
        if el == *ancestor {
            return true;
        }
        cur = el.parent_element();
    }
    false
}

/// `"r,g,b"` → `[u8; 3]` (each part 0..=255).
fn parse_color_triple(s: &str) -> Option<[u8; 3]> {
    let parts: Vec<u8> = s
        .split(',')
        .map(|p| p.trim().parse::<u8>().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.len() != 3 {
        return None;
    }
    Some([parts[0], parts[1], parts[2]])
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    let window: Window = web_sys::window().expect("no window");
    let document = window.document().expect("no document");

    let canvas = document
        .get_element_by_id("game")
        .and_then(|e| e.dyn_into::<HtmlCanvasElement>().ok())
        .expect("missing #game canvas");
    let hud = document
        .get_element_by_id("hud")
        .and_then(|e| e.dyn_into::<HtmlDivElement>().ok())
        .expect("missing #hud");
    let overlay = document
        .get_element_by_id("overlay")
        .and_then(|e| e.dyn_into::<HtmlDivElement>().ok())
        .expect("missing #overlay");

    let (seed, verify_mode, walk_mode, tag_log, npcs, server_url_param) = params_from_url();
    log(&format!("RustCraft: app started (seed {seed})"));

    let server_input = document
        .get_element_by_id("server-url")
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        .expect("missing #server-url");
    let server_button = document
        .get_element_by_id("server-connect")
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        .expect("missing #server-connect");
    let server_status = document
        .get_element_by_id("server-status")
        .and_then(|e| e.dyn_into::<HtmlDivElement>().ok())
        .expect("missing #server-status");
    let options = document
        .get_element_by_id("options")
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
        .expect("missing #options");
    let options_button = document
        .get_element_by_id("options-btn")
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
        .expect("missing #options-btn");
    let name_input = document
        .get_element_by_id("player-name")
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        .expect("missing #player-name");
    let palette = document
        .get_element_by_id("palette")
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
        .expect("missing #palette");
    let builtin_button = document
        .get_element_by_id("server-builtin")
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
        .expect("missing #server-builtin");
    let tags = document
        .get_element_by_id("tags")
        .and_then(|e| e.dyn_into::<HtmlDivElement>().ok())
        .expect("missing #tags");

    // In verify mode the headless test never gets pointer lock, so hide the
    // "click to play" overlay to let screenshots show the rendered canvas.
    if verify_mode {
        overlay.style().set_property("display", "none").ok();
        if let Some(x) = document
            .get_element_by_id("crosshair")
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        {
            x.style().set_property("display", "none").ok();
        }
    }

    let app = Rc::new(RefCell::new(App::new(
        seed,
        hud.clone(),
        overlay.clone(),
        server_status.clone(),
        tags.clone(),
        verify_mode,
        walk_mode,
        tag_log,
        npcs,
    )));
    if let Some((c, s)) = npcs {
        log(&format!("RustCraft: NPC load test armed: {c} agents @ {s:.0} m spacing"));
    }
    // Server-connect panel: pre-fill from ?server= and connect right away
    // (the headless-test path); otherwise the user drives it from the UI.
    if let Some(url) = server_url_param.as_deref() {
        server_input.set_value(url);
        log(&format!("RustCraft: auto-connecting to {url}"));
        connect_remote(&app, url);
    }

    // ---- Input events ----------------------------------------------------
    // Each closure captures its own clone of `app`; the outer Rc is never
    // moved. Closures are kept alive via raw pointers in App::_closures.
    {
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new({
            let app = app.clone();
            move |e: KeyboardEvent| {
                // Typing in a text field (player name, server URL) must
                // never feed game input: no key capture, and — crucially —
                // no preventDefault, which would swallow the character
                // before it reaches the input (w/a/s/d are all game keys).
                if event_target_is_text_input(&e) {
                    return;
                }
                match key_from_code(e.code().as_str()) {
                    // One-shot fly actions (F must ignore key auto-repeat;
                    // E/Q may repeat so holding them ramps the speed).
                    Some(Key::F) if !e.repeat() => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::ToggleFly);
                    }
                    Some(Key::E) => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::FlyFaster);
                    }
                    Some(Key::Q) => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::FlySlower);
                    }
                    // NPC load test: N/C are one-shot (ignore auto-repeat);
                    // I/U and [ ] may repeat so holding them ramps the dial.
                    Some(Key::KeyN) if !e.repeat() => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::NpcLoad);
                    }
                    Some(Key::KeyC) if !e.repeat() => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::NpcClear);
                    }
                    Some(Key::KeyI) => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::NpcCountUp);
                    }
                    Some(Key::KeyU) => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::NpcCountDown);
                    }
                    Some(Key::BracketLeft) => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::NpcSpacingDown);
                    }
                    Some(Key::BracketRight) => {
                        e.prevent_default();
                        app.borrow_mut().actions.push(Action::NpcSpacingUp);
                    }
                    Some(k) => {
                        e.prevent_default();
                        app.borrow_mut().keys.insert(k);
                    }
                    None => {}
                }
            }
        });
        window
            .add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())
            .expect("keydown listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new({
            let app = app.clone();
            move |e: KeyboardEvent| {
                if event_target_is_text_input(&e) {
                    return; // the input's own handlers own these keys
                }
                if let Some(k) = key_from_code(e.code().as_str()) {
                    app.borrow_mut().keys.remove(k);
                }
            }
        });
        window
            .add_event_listener_with_callback("keyup", cb.as_ref().unchecked_ref())
            .expect("keyup listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        let cb = Closure::<dyn FnMut(MouseEvent)>::new({
            let app = app.clone();
            move |e: MouseEvent| {
                let mut a = app.borrow_mut();
                if !a.locked {
                    return;
                }
                let (yaw, pitch) = (a.aim_yaw, a.aim_pitch);
                match e.button() {
                    0 => a.actions.push(Action::Break { yaw, pitch }),
                    2 => a.actions.push(Action::Place { yaw, pitch }),
                    _ => {}
                }
            }
        });
        canvas
            .add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref())
            .expect("mousedown listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        let cb = Closure::<dyn FnMut(MouseEvent)>::new({
            let app = app.clone();
            move |e: MouseEvent| {
                let mut a = app.borrow_mut();
                if a.locked {
                    a.mouse_dx += e.movement_x() as f32;
                    a.mouse_dy += e.movement_y() as f32;
                }
            }
        });
        window
            .add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())
            .expect("mousemove listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Click anywhere to (re)enter pointer lock. The listener is on
        // `document`, not the canvas, because the click-to-play overlay
        // covers the canvas — a canvas-only listener would never fire while
        // the menu is up. Clicks inside the options panel (name/colour/
        // server controls) are exempt: they need focused inputs and real
        // button clicks, not a pointer lock.
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new({
            let app = app.clone();
            let canvas_for_lock = canvas.clone();
            let options_ref = options.clone();
            move |e: web_sys::Event| {
                if e
                    .target()
                    .and_then(|t| t.dyn_ref::<Element>().cloned())
                    .is_some_and(|t| is_inside(&t, &options_ref))
                {
                    return;
                }
                let a = app.borrow();
                if !a.locked {
                    let _ = canvas_for_lock.request_pointer_lock();
                }
            }
        });
        document
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .expect("click listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Server-connect panel: the URL field never feeds game input
        // (typing it must not move the player), Enter connects.
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new({
            let app = app.clone();
            let input_el = server_input.clone();
            move |e: KeyboardEvent| {
                e.stop_propagation();
                if e.code() == "Enter" {
                    let url = input_el.value().trim().to_string();
                    if !url.is_empty() {
                        connect_remote(&app, &url);
                    }
                }
            }
        });
        server_input
            .add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())
            .expect("server-url keydown listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Connect button: non-empty field connects, empty field falls back
        // to the built-in server.
        let cb = Closure::<dyn FnMut()>::new({
            let app = app.clone();
            let input_el = server_input.clone();
            move || {
                let url = input_el.value().trim().to_string();
                if url.is_empty() {
                    let mut a = app.borrow_mut();
                    let seed = a.builtin_seed;
                    a.fallback_to_builtin();
                    a.set_server_status(&format!("built-in server (seed {seed})"));
                } else {
                    connect_remote(&app, &url);
                }
            }
        });
        server_button
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .expect("server-connect click listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Options panel: the "Options" button toggles its open state.
        let cb = Closure::<dyn FnMut()>::new({
            let options_ref = options.clone();
            move || {
                let _ = options_ref.class_list().toggle("open");
            }
        });
        options_button
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .expect("options button click listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Player name: typing never feeds game input (stop propagation),
        // and every change is pushed to the backend (the shared world
        // broadcasts it to the other players' name tags).
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new({
            let app = app.clone();
            let input_el = name_input.clone();
            move |e: KeyboardEvent| {
                e.stop_propagation();
                let mut a = app.borrow_mut();
                let raw = input_el.value().trim().to_string();
                a.player_name = if raw.is_empty() { "Player".to_string() } else { raw };
                a.apply_profile();
            }
        });
        name_input
            .add_event_listener_with_callback("input", cb.as_ref().unchecked_ref())
            .expect("player-name input listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Colour palette: one swatch per fixed colour; clicking picks it
        // (selection ring) and pushes the profile to the backend.
        {
            let kids = palette.children();
            let mut swatches: Vec<HtmlElement> = Vec::new();
            let mut i = 0;
            while i < kids.length() {
                if let Some(c) = kids.item(i) {
                    if let Ok(h) = c.dyn_into::<HtmlElement>() {
                        swatches.push(h);
                    }
                }
                i += 1;
            }
            for s in &swatches {
                if s
                    .get_attribute("data-color")
                    .as_deref()
                    .is_some_and(|v| v == "255,255,255")
                {
                    let _ = s.class_list().add_1("selected"); // default white
                }
            }
            let cb = Closure::<dyn FnMut(MouseEvent)>::new({
                let app = app.clone();
                let swatches = swatches.clone();
                move |e: MouseEvent| {
                    let Some(swatch) = e
                        .target()
                        .and_then(|t| t.dyn_ref::<Element>().cloned())
                        .and_then(|el| el.closest(".swatch").ok().flatten())
                    else {
                        return;
                    };
                    let Some(color) = swatch
                        .get_attribute("data-color")
                        .and_then(|v| parse_color_triple(&v))
                    else {
                        return;
                    };
                    for s in &swatches {
                        let _ = s.class_list().remove_1("selected");
                    }
                    let _ = swatch.class_list().add_1("selected");
                    let mut a = app.borrow_mut();
                    a.player_color = color;
                    a.apply_profile();
                }
            });
            palette
                .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                .expect("palette click listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);
        }

        // "Use built-in server": drop any remote link and return to a
        // fresh embedded world (same as Connect with an empty URL).
        let cb = Closure::<dyn FnMut()>::new({
            let app = app.clone();
            move || {
                let mut a = app.borrow_mut();
                let seed = a.builtin_seed;
                a.fallback_to_builtin();
                a.set_server_status(&format!("built-in server (seed {seed})"));
            }
        });
        builtin_button
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .expect("built-in button click listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Pointer lock state.
        let cb = Closure::<dyn FnMut()>::new({
            let app = app.clone();
            let doc = document.clone();
            let canvas_ref = canvas.clone();
            let overlay_ref = overlay.clone();
            move || {
                let locked = doc.pointer_lock_element().as_ref() == Some(&canvas_ref);
                let mut a = app.borrow_mut();
                a.locked = locked;
                let _ = overlay_ref
                    .style()
                    .set_property("display", if locked { "none" } else { "block" });
                if !locked {
                    a.keys = KeySet::default();
                }
            }
        });
        document
            .add_event_listener_with_callback("pointerlockchange", cb.as_ref().unchecked_ref())
            .expect("pointerlockchange listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Suppress the context menu on the canvas.
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
            e.prevent_default();
        });
        canvas
            .add_event_listener_with_callback("contextmenu", cb.as_ref().unchecked_ref())
            .expect("contextmenu listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);

        // Window resize.
        let cb = Closure::<dyn FnMut()>::new({
            let app = app.clone();
            let canvas_ref = canvas.clone();
            move || {
                let mut a = app.borrow_mut();
                if let Some(r) = &mut a.renderer {
                    r.resize(&canvas_ref);
                }
            }
        });
        window
            .add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref())
            .expect("resize listener");
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);
    }

    // ---- Async renderer init, then main loop ------------------------------
    let app_render = app.clone();
    let canvas_render = canvas.clone();
    spawn_local(async move {
        match Renderer::new(&canvas_render).await {
            Ok(renderer) => {
                app_render.borrow_mut().renderer = Some(renderer);
                log("RustCraft: renderer ready");
            }
            Err(e) => {
                log(&format!("RustCraft: renderer init failed: {e}"));
                web_sys::console::error_1(&JsValue::from_str(&e));
                let mut msg = String::from("WebGPU failed: ");
                msg.push_str(&e);
                app_render.borrow_mut().overlay.set_text_content(Some(&msg));
            }
        }
    });

    // rAF loop. The closure is kept alive in a holder and re-schedules itself
    // through that holder (wasm closures can't reference themselves directly).
    let app_loop = app.clone();
    let raf: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    {
        let app_loop = app_loop.clone();
        let raf_cb = raf.clone();
        let win = window.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            app_loop.borrow_mut().frame();
            if let Some(cb) = raf_cb.borrow().as_ref() {
                let _ = win.request_animation_frame(cb.as_ref().unchecked_ref());
            }
        });
        *raf.borrow_mut() = Some(cb);
    }
    window
        .request_animation_frame(raf.borrow().as_ref().unwrap().as_ref().unchecked_ref())
        .expect("request_animation_frame");

    // Fallback driver: in some headless environments requestAnimationFrame
    // never fires; a 16 ms interval keeps the simulation and renderer going.
    {
        let app_loop = app_loop.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            app_loop.borrow_mut().frame();
        });
        let win = window.clone();
        let _interval =
            win.set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 16);
        app.borrow_mut()
            ._closures
            .push(Box::into_raw(Box::new(cb)) as *mut _);
    }

    Ok(())
}
