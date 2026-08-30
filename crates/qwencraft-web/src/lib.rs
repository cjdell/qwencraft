//! Qwencraft browser entry point.
//!
//! Two server backends:
//! - **built-in** (default): the game server is embedded in this wasm module
//!   and driven directly from the browser event loop;
//! - **remote**: a headless server (`qwencraft-net`) served over WebSocket —
//!   pointed at via the overlay's connect panel or `?server=ws://host:port`.
//!   The client renders server state and forwards input; it never mutates
//!   world state (the server stays authoritative in both modes).
//!
//! This crate only compiles for wasm32 (browser entry point).

#![cfg(target_arch = "wasm32")]

#[cfg(feature = "verify")]
mod verify_gl;

use std::cell::RefCell;
#[cfg(feature = "verify")]
use std::collections::HashMap; // verify_regions (the shadow renderer's input)
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    Element, HtmlCanvasElement, HtmlDivElement, HtmlElement, HtmlInputElement, KeyboardEvent,
    MessageEvent, MouseEvent, Touch, TouchEvent, TouchList, Window, WheelEvent,
};

use js_sys::Function;

use qwencraft_client::Renderer;
use qwencraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use qwencraft_server::{
    Action, AgentState, Input, Key, KeySet, Server, ServerStats, Streamer, Vec3, WorldUpdate,
};
use qwencraft_world::camera::view_projection;
use qwencraft_world::{Block, BlockPos, BLOCKS, ChunkPos, PLACEABLE};

/// Number of hotbar slots (the 9 placeable-block window the player can
/// select with the digit keys / mouse wheel).
const HOTBAR_SLOTS: usize = 9;

// Transit-loss reconciliation thresholds (the Stats branch of
// `apply_remote_messages`): if the server's per-viewer send count is more
// than RESYNC_GAP_CHUNKS beyond the distinct chunks we actually hold, and
// nothing has arrived for RESYNC_STALE_MS, the gap is a loss (a healthy
// but slow link keeps chunks arriving, so it can't false-positive); a
// resync is at most once per RESYNC_COOLDOWN_MS (a spurious one is a
// no-op: the server only re-queues what we don't have).
const RESYNC_GAP_CHUNKS: i64 = 32;
const RESYNC_STALE_MS: f64 = 5000.0;
const RESYNC_COOLDOWN_MS: f64 = 10_000.0;

// "Server is from the future": if the Hello version is NEWER than ours,
// this page's assets are stale — the headless server serves the build
// matching its own protocol, so a version skew can only mean the browser
// handed us an old cached copy. `force_reload_cache_busted` reloads the
// page with a unique query param (busting the document in every cache,
// intermediaries included) + unregisters any service worker, under a
// per-tab budget (sessionStorage) so a permanently stale link can't loop
// the page forever (see `protocol::future_reload_budget`).
const FUTURE_RELOAD_KEY: &str = "qwc_future_reload";

fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

/// Result of a console `getBlock` request (see `Backend::console_get_block`).
enum ConsoleGetBlock {
    /// Built-in: the authoritative answer, synchronously.
    Answered(Block),
    /// Remote: the request was sent; the answer arrives as `BlockAt`.
    RequestSent,
    /// Remote: no live connection (yet).
    NotConnected,
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
                        analog_x: input.analog_x,
                        analog_y: input.analog_y,
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

    /// Console `qwc.getBlock`: the built-in backend answers synchronously
    /// from the embedded world; the remote backend round-trips the server
    /// (the answer arrives as `ServerMsg::BlockAt`). Reads are never
    /// answered from this client's own streamed copy — the server stays
    /// the source of truth (golden rule 4).
    fn console_get_block(&mut self, pos: BlockPos) -> ConsoleGetBlock {
        match self {
            Backend::Builtin { server, .. } => ConsoleGetBlock::Answered(server.block_at(pos)),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::GetBlock { pos });
                    ConsoleGetBlock::RequestSent
                } else {
                    ConsoleGetBlock::NotConnected
                }
            }
        }
    }

    /// Console `qwc.setBlock`: built-in applies it to the embedded server;
    /// remote sends `SetBlock` (the server applies it on the same
    /// world-write path as a player edit and re-sends the dirty chunks to
    /// every viewer that holds them on the next tick).
    fn console_set_block(&mut self, pos: BlockPos, block: Block) -> Result<(), String> {
        match self {
            Backend::Builtin { server, .. } => server.console_edit_block(0, pos, block),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::SetBlock {
                        pos,
                        block: block.as_u8(),
                    });
                    Ok(())
                } else {
                    Err("not connected to a server".to_string())
                }
            }
        }
    }

    /// Console `qwc.setPlayerPos`: built-in teleports the embedded player;
    /// remote sends `Teleport` (the server clamps y into the world and
    /// zeroes velocity — the next PlayerState carries the new position).
    fn console_teleport(&mut self, pos: Vec3) -> Result<(), String> {
        match self {
            Backend::Builtin { server, .. } => server.console_teleport(0, pos),
            Backend::Remote(r) => {
                if r.connected {
                    r.send(ClientMsg::Teleport { pos });
                    Ok(())
                } else {
                    Err("not connected to a server".to_string())
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
    /// Every chunk position received from this server (de-duplicated).
    /// Transit-loss detection: the server's per-viewer `chunks_sent`
    /// (Stats) must stay within in-flight margin of this set — a growing
    /// gap with no arrivals means a burst was lost in flight (see the
    /// Stats branch of `apply_remote_messages` and `ClientMsg::Resync`).
    have: std::collections::HashSet<ChunkPos>,
    /// Wall-clock ms (`js_sys::Date`) of the last chunk message received.
    last_chunk_ms: f64,
    /// Wall-clock ms of the last `Resync` sent (cooldown).
    last_resync_ms: f64,
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
    /// Chunk updates produced while the renderer was still initialising.
    /// WebGPU device creation is async, and on a slow GPU (an Intel Xe
    /// iGPU on Linux, or a SwiftShader fallback) it can outlast the first
    /// streaming pass. They must be applied — in order — the moment the
    /// pool exists: the streamer marks chunks sent when it queues them
    /// (built-in) and the client records them in `have` on socket receipt
    /// (remote), so dropping them would leave the spawn view a permanent
    /// hole that is never re-sent.
    pending_updates: Vec<WorldUpdate>,
    /// The player's spawn xz (built-in: at world creation; remote: the
    /// first PlayerState after Hello). Anchors the POOL line's
    /// `spawn_near` telemetry — the 3x3-chunk box the streamer sends
    /// first (nearest-first), which must be in the pool.
    spawn_xz: Option<[f32; 2]>,
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
    /// Coarse-pointer device (or forced by `?touchtest=1`): the two thumb
    /// pads replace pointer lock (no touch device → desktop controls).
    touch_mode: bool,
    /// Move pad (left thumb): stick vector, x = right, y = forward,
    /// magnitude ≤ 1. Sent as the analog part of the per-frame Input (the
    /// server scales walk speed by it — the stick's distance from centre
    /// is the throttle).
    joy_x: f32,
    joy_y: f32,
    last_time: f64,
    frames: u32,
    fps_time: f64,
    fps: f32,
    // Last frame timestamp (ms) — guards against double-driving from both
    // the rAF loop and the setInterval fallback.
    last_frame_ms: f64,
    // Verify mode (?verify=1): accumulate the streamed chunk regions and
    // re-render them through a WebGL2 "shadow" renderer whose pixels can be
    // read back even in headless browsers (see verify_gl.rs, `verify` feature).
    #[cfg(feature = "verify")]
    verify_mode: bool,
    /// Headless stress test: hold W and walk (turning away when blocked by
    /// terrain), exercising terrain streaming + pool compaction.
    walk_mode: bool,
    /// `?taglog=1`: log name-tag screen positions every 5 s (headless
    /// verification that tags track their players).
    tag_log: bool,
    /// `?dbg=1`: verbose chunk-receive/eviction trace (WAN debugging).
    dbg: bool,
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
    #[cfg(feature = "verify")]
    verify_done: bool,
    // Per-phase frame timings (ms), accumulated since the last HUD update.
    perf_tick_ms: f64,
    perf_mesh_ms: f64,
    perf_render_ms: f64,
    hud_updates: u32,
    #[cfg(feature = "verify")]
    verify_regions: HashMap<ChunkPos, Vec<u8>>,
    #[cfg(feature = "verify")]
    gl_verify: Option<verify_gl::GlVerifier>,

    // Remote-server bookkeeping (the live link itself lives in `backend`).
    next_link_id: u32,
    /// A cache-busting "server is from the future" reload is in flight —
    /// the link's close handler must not fall back to the built-in while
    /// the page is reloading.
    future_reloading: bool,
    /// Pending `qwc.getBlock` round-trips (remote mode): (owning link id,
    /// requested position, promise). Settled FIFO per position when the
    /// `BlockAt` answer arrives; rejected when the link dies (a replaced
    /// or dropped link will never answer them).
    pending_blocks: Vec<(u32, BlockPos, JsPromise)>,

    /// NPC load armed via `?npcs=`, applied once a remote connection says
    /// Hello (the built-in backend already got it in `App::new`).
    pending_npcs: Option<(u32, f32)>,
    /// Status line of the overlay's server-connect panel.
    server_status: HtmlDivElement,
    // ---- Hotbar (block selection) ---------------------------------------
    /// Selected hotbar slot (0-based into `PLACEABLE`'s first 9 entries).
    selected_slot: usize,
    /// The hotbar container (slots are built into it at startup).
    hotbar: HtmlDivElement,
    /// One slot element per `HOTBAR_SLOTS` (class "selected" marks the
    /// active one).
    hotbar_slots: Vec<HtmlElement>,
    /// Label showing the selected block's name above the hotbar.
    hotbar_name: HtmlDivElement,
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
        hotbar: HtmlDivElement,
        hotbar_name: HtmlDivElement,
        verify_mode: bool,
        walk_mode: bool,
        tag_log: bool,
        dbg: bool,
        npcs: Option<(u32, f32)>,
    ) -> Self {
        #[cfg(not(feature = "verify"))]
        let _ = verify_mode; // `verify` feature off: the parameter is ignored.
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
            dbg,
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
            pending_updates: Vec::new(),
            spawn_xz: Some([spawn.x, spawn.z]),
            hud,
            overlay,
            keys: KeySet::default(),
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            actions,
            aim_yaw: 0.0,
            aim_pitch: 0.0,
            locked: false,
            touch_mode: false,
            joy_x: 0.0,
            joy_y: 0.0,
            last_time: 0.0,
            frames: 0,
            fps_time: 0.0,
            fps: 0.0,
            last_frame_ms: 0.0,
            #[cfg(feature = "verify")]
            verify_mode,
            frames_total: 0,
            #[cfg(feature = "verify")]
            verify_done: false,
            perf_tick_ms: 0.0,
            perf_mesh_ms: 0.0,
            perf_render_ms: 0.0,
            hud_updates: 0,
            #[cfg(feature = "verify")]
            verify_regions: HashMap::new(),
            #[cfg(feature = "verify")]
            gl_verify: None,
            next_link_id: 0,
            future_reloading: false,
            pending_blocks: Vec::new(),
            pending_npcs: npcs,
            server_status,
            player_name: "Player".to_string(),
            player_color: [255, 255, 255],
            tags,
            tag_els: std::collections::HashMap::new(),
            selected_slot: 0,
            hotbar,
            hotbar_slots: Vec::new(),
            hotbar_name,
            _closures: Vec::new(),
        }
    }

    /// Build the hotbar's slots (one per `PLACEABLE` entry, up to
    /// `HOTBAR_SLOTS`) and mark slot 0 selected. Slot backgrounds use the
    /// block's CPU fallback colour (the texture's average — close enough
    /// for a UI swatch; the real texture lives in the shader).
    fn build_hotbar(&mut self) {
        let doc = web_sys::window().expect("window").document().expect("document");
        let mut slots = Vec::new();
        for (i, block) in PLACEABLE.iter().take(HOTBAR_SLOTS).enumerate() {
            let el: HtmlElement = doc
                .create_element("div")
                .expect("div")
                .dyn_into()
                .expect("div");
            el.set_class_name("hotbar-slot");
            let c = block.color_top();
            el.style()
                .set_property(
                    "background",
                    &format!(
                        "rgb({},{},{})",
                        (c[0] * 255.0) as u8,
                        (c[1] * 255.0) as u8,
                        (c[2] * 255.0) as u8
                    ),
                )
                .ok();
            let _ = el.set_attribute("data-block", &block.as_u8().to_string());
            let _ = el.set_attribute("data-slot", &i.to_string());
            let _ = el.set_attribute("title", block.info().name);
            if i == 0 {
                let _ = el.class_list().add_1("selected");
            }
            self.hotbar.append_child(&el).expect("append slot");
            slots.push(el);
        }
        self.hotbar_slots = slots;
        self.set_hotbar_label();
    }

    /// Refresh the selected-block label above the hotbar.
    fn set_hotbar_label(&mut self) {
        let b = PLACEABLE[self.selected_slot];
        self.hotbar_name.set_text_content(Some(b.info().name));
    }

    /// Select hotbar slot `i` (clamped); updates the DOM + label.
    fn select_slot(&mut self, i: usize) {
        if i >= HOTBAR_SLOTS {
            return;
        }
        if i == self.selected_slot {
            return;
        }
        for (j, el) in self.hotbar_slots.iter().enumerate() {
            if j == i {
                let _ = el.class_list().add_1("selected");
            } else {
                let _ = el.class_list().remove_1("selected");
            }
        }
        self.selected_slot = i;
        self.set_hotbar_label();
    }

    /// The block the player currently has selected (right-click places it).
    fn selected_block(&self) -> Block {
        PLACEABLE[self.selected_slot]
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
                    log(&format!("Qwencraft: name tag created for player {} (\"{}\")", s.id, label));
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

    /// Reject every pending `qwc.getBlock` promise owned by remote link
    /// `id` (a closed or replaced link will never answer them).
    fn reject_pending_blocks(&mut self, link_id: u32, reason: &str) {
        let mut i = 0;
        while i < self.pending_blocks.len() {
            if self.pending_blocks[i].0 == link_id {
                let (_, _, promise) = self.pending_blocks.remove(i);
                promise.reject(reason);
            } else {
                i += 1;
            }
        }
    }

    /// Update the connect panel's status line (and the console log).
    fn set_server_status(&mut self, msg: &str) {
        self.server_status.set_text_content(Some(msg));
        log(&format!("Qwencraft: server: {msg}"));
    }

    /// Drop any remote link and return to a fresh built-in server.
    fn fallback_to_builtin(&mut self) {
        let server = Server::new(self.builtin_seed);
        let spawn = server.player_state().pos;
        self.backend = Backend::Builtin { server, streamer: Streamer::new() };
        self.spawn_xz = Some([spawn.x, spawn.z]);
        // The previous world's terrain belongs to the old backend.
        if let Some(r) = self.renderer.as_mut() {
            r.clear_terrain();
        }
        self.keys = KeySet::default();
    }

    /// Apply decoded server messages to the remote link `id`.
    fn apply_remote_messages(&mut self, id: u32, msgs: Vec<ServerMsg>, url: &str) {
        let mut hello_seed: Option<u64> = None;
        // `BlockAt` answers are collected here and settle the matching
        // pending `qwc.getBlock` promises after the link's state is done
        // (the pending list lives on the App, not the link).
        let mut block_ats: Vec<(BlockPos, u8)> = Vec::new();
        if let Backend::Remote(r) = &mut self.backend {
            if r.id != id {
                return;
            }
            for m in msgs {
                match m {
                    ServerMsg::Hello { version, seed, player_id } => {
                        if version != PROTOCOL_VERSION {
                            let future = version > PROTOCOL_VERSION;
                            let _ = r.ws.close();
                            let mut reloaded = false;
                            if future {
                                // Server is from the future: this page's
                                // assets are stale — force a cache-busting
                                // reload. If the per-tab budget is
                                // exhausted, fall back to the built-in
                                // with a clear message instead of reloading
                                // forever.
                                log(&format!(
                                    "Qwencraft: server speaks protocol {version}, client has {PROTOCOL_VERSION} — server is newer than this page"
                                ));
                                reloaded = force_reload_cache_busted();
                            } else {
                                log(&format!(
                                    "Qwencraft: server speaks protocol {version}, client has {PROTOCOL_VERSION} — closing"
                                ));
                            }
                            // `r`'s last use was above; the rest works on
                            // `self` directly.
                            if reloaded {
                                self.future_reloading = true;
                                self.set_server_status(
                                    "server is newer than this page — reloading with cache busting…",
                                );
                            } else if future {
                                self.set_server_status(
                                    "server is newer than this page and cache-bust reloads failed — built-in server (hard-reload the page)",
                                );
                                self.fallback_to_builtin();
                            }
                            // Else: the close handler does the fallback.
                            return;
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
                            "Qwencraft: remote server connected (seed {seed}, player {player_id})"
                        ));
                    }
                    ServerMsg::PlayerState(s) => {
                        // The first state after Hello is the spawn position
                        // (the player can't have moved yet): anchor for the
                        // POOL line's spawn_near telemetry.
                        if self.spawn_xz.is_none() {
                            self.spawn_xz = Some([s.pos.x, s.pos.z]);
                        }
                        r.player = s;
                    }
                    ServerMsg::Agents(v) => r.agents = v,
                    ServerMsg::Chunk { pos, data } => {
                        if self.dbg {
                            log(&format!("DBG recv chunk ({},{},{})", pos.x, pos.y, pos.z));
                        }
                        // Transit-loss bookkeeping (see `have`'s docs).
                        r.have.insert(pos);
                        r.last_chunk_ms = js_sys::Date::now();
                        r.inbound.push(WorldUpdate::Chunk { pos, data })
                    }
                    ServerMsg::Stats(s) => {
                        r.stats = s;
                        // Transit-loss reconciliation (see `have`'s docs):
                        // the server's per-viewer send count and the
                        // distinct chunks we actually hold must stay within
                        // in-flight margin. A large gap that PERSISTS with
                        // no chunk arrivals means the burst was lost in
                        // flight — request a resync; the server re-sends
                        // every ready chunk in view we don't have.
                        // Keep `have` bounded over long sessions: it only
                        // ever matters within the view (the server's resync
                        // window), so forget the rest once it grows large.
                        if r.have.len() > 8192 {
                            let cell = [
                                (r.player.pos.x / 16.0).floor() as i32,
                                (r.player.pos.y / 16.0).floor() as i32,
                                (r.player.pos.z / 16.0).floor() as i32,
                            ];
                            r.have.retain(|c| {
                                (c.x - cell[0])
                                    .abs()
                                    .max((c.y - cell[1]).abs())
                                    .max((c.z - cell[2]).abs())
                                    <= qwencraft_server::VIEW_RADIUS + 2
                            });
                        }
                        let now = js_sys::Date::now();
                        let gap = s.chunks_sent as i64 - r.have.len() as i64;
                        if r.connected
                            && gap > RESYNC_GAP_CHUNKS
                            && now - r.last_chunk_ms > RESYNC_STALE_MS
                            && now - r.last_resync_ms > RESYNC_COOLDOWN_MS
                        {
                            let have: Vec<ChunkPos> = r.have.iter().copied().collect();
                            r.last_resync_ms = now;
                            log(&format!(
                                "Qwencraft: {} chunks missing (server sent {}, holding {}) and none arrived for {:.0}s — requesting resync",
                                gap,
                                s.chunks_sent,
                                have.len(),
                                (now - r.last_chunk_ms) / 1000.0
                            ));
                            r.send(ClientMsg::Resync(have));
                        }
                    }
                    ServerMsg::NpcLoad { count, spacing } => r.npc_load = (count, spacing),
                    ServerMsg::BlockAt { pos, block } => block_ats.push((pos, block)),
                }
            }
        } else {
            return;
        }
        // Settle the pending `qwc.getBlock` promises (FIFO per requested
        // position: an answer matches the oldest outstanding request for
        // that exact position on this link).
        for (pos, block) in block_ats {
            if let Some(i) = self
                .pending_blocks
                .iter()
                .position(|(link, p, _)| *link == id && *p == pos)
            {
                let (_, _, promise) = self.pending_blocks.remove(i);
                promise.resolve(block_obj(pos, Block::from_u8(block)));
            }
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
    /// `verify` feature only (the shadow renderer is not built without it).
    #[cfg(feature = "verify")]
    fn run_gl_verify(&mut self) {
        let p = self.backend.player_state();
        let cam = [p.pos.x, p.pos.y + qwencraft_server::agent::EYE_HEIGHT, p.pos.z];
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
        // Touch joystick (the move pad): the server scales walk speed by
        // the stick's distance from centre (throttle); keyboard clients
        // always send (0, 0) here and the keys drive movement.
        input.analog_x = self.joy_x;
        input.analog_y = self.joy_y;
        self.backend.set_input(input);
        for a in self.actions.drain(..) {
            self.backend.push_action(a);
        }
        let t_tick = js_sys::Date::now();
        self.backend.tick(dt);
        let t_mesh = js_sys::Date::now();

        // Updates buffered while the renderer was still initialising come
        // first (they are the oldest): applying them now closes the
        // spawn-view hole that dropping them would make permanent (see
        // `pending_updates`).
        let mut updates = std::mem::take(&mut self.pending_updates);
        let buffered = updates.len();
        updates.extend(self.backend.take_world_updates());
        if self.dbg && !updates.is_empty() {
            log(&format!("DBG frame: {} chunks to mesh", updates.len()));
        }
        #[cfg(feature = "verify")]
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
            player.pos.y + qwencraft_server::agent::EYE_HEIGHT,
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
            if buffered > 0 {
                log(&format!(
                    "Qwencraft: applying {} chunks buffered during renderer init",
                    buffered
                ));
            }
            r.apply_updates(updates);
            // Report every chunk the pool evicted (visible or fog-bound).
            // The streamer forgets them and re-sends the ones that are
            // visible again (its normal stream: nearest-first, with
            // lookahead). Without this, chunks evicted while far away —
            // fully fogged, dropped silently — would stay holes when the
            // player walks back over the terrain.
            let evicted = r.take_evicted();
            if !evicted.is_empty() {
                if self.dbg {
                    let list: Vec<String> = evicted
                        .iter()
                        .map(|c| format!("({},{},{})", c.x, c.y, c.z))
                        .collect();
                    log(&format!(
                        "DBG evict {} cam=({:.0},{:.0},{:.0}): {}",
                        evicted.len(),
                        player.pos.x,
                        player.pos.y,
                        player.pos.z,
                        list.join(" ")
                    ));
                }
                self.backend.report_evicted(evicted);
            }
            let t_render = js_sys::Date::now();
            // All agents except our own (rendered first person): other
            // players are spheres like NPCs.
            r.set_agents(agents, own_id);
            r.set_highlight(player.target.map(|t| [t.x, t.y, t.z]));
            // Wall-clock seconds drive the water texture's ripples.
            r.render(cam, player.yaw, player.pitch, now as f32);
            let t_done = js_sys::Date::now();
            self.perf_tick_ms += t_mesh - t_tick;
            self.perf_mesh_ms += t_render - t_mesh;
            self.perf_render_ms += t_done - t_render;
            if r.take_first_frame() {
                log("Qwencraft: first frame rendered");
            }
            self.frames_total += 1;
            #[cfg(feature = "verify")]
            if self.verify_mode && self.frames_total == 410 && !self.verify_done {
                // Plenty of chunks have been streamed and meshed by now;
                // run the WebGL2 shadow readback + one-shot screenshot.
                self.run_gl_verify();
            }
        } else {
            // No renderer yet (device still initialising): hold the
            // updates for the first rendered frame. Dropping them was the
            // "invisible spawn" bug — the streamer has already marked them
            // sent (built-in) and they are already in `have` (remote), so
            // they would never be re-sent.
            self.pending_updates.extend(updates);
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
                    // `sent` is the server-side per-viewer count (remote) or
                    // the local streamer's count (built-in). `sent - chunks`
                    // is NOT a loss metric: fully-air/buried chunks are sent
                    // but have no mesh, so the gap is normally large (tens
                    // of chunks). The "invisible spawn" signature is
                    // `spawn_near=0` while `sent` grows — the nearest-first
                    // initial burst (the spawn area) never reached the pool.
                    log(&format!(
                        "POOL chunks={} missing={} sent={} agents={} free={} spawn_near={}",
                        r.chunk_count(),
                        missing,
                        stats.chunks_sent,
                        stats.agents,
                        r.free_slots(),
                        self
                            .spawn_xz
                            .map(|s| r.spawn_near_count(s))
                            .unwrap_or(0)
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
        a.future_reloading = false;
        // The new world's spawn is unknown until its first PlayerState
        // arrives (it anchors the POOL line's spawn_near telemetry).
        a.spawn_xz = None;
        a.backend = Backend::Remote(RemoteLink {
            id,
            ws: ws.clone(),
            url: url.clone(),
            connected: false,
            player_id: u32::MAX,
            seed: None,
            have: std::collections::HashSet::new(),
            last_chunk_ms: 0.0,
            last_resync_ms: 0.0,
            inbound: Vec::new(),
            player: AgentState::default(),
            agents: Vec::new(),
            stats: ServerStats::default(),
            npc_load: (
                qwencraft_server::NPC_COUNT_DEFAULT,
                qwencraft_server::NPC_SPACING_DEFAULT,
            ),
        });
        // The old world's terrain belongs to the old backend.
        if let Some(r) = a.renderer.as_mut() {
            r.clear_terrain();
        }
        a.set_server_status(&format!("connecting to {url} …"));
        id
    };
    log(&format!("Qwencraft: connecting to {url}"));

    // open
    {
        let app_cb = app.clone();
        let url_cb = url.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            if app_cb.borrow().remote_id() != id {
                return;
            }
            log(&format!("Qwencraft: remote socket open: {url_cb}"));
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
            // Settle any `qwc.getBlock` promises this link will never
            // answer. Done BEFORE the id check: the close also fires when
            // the link is replaced by a new connection, which skips the
            // rest of this handler.
            a.reject_pending_blocks(id, "connection lost");
            if a.remote_id() != id {
                return;
            }
            if a.future_reloading {
                // A cache-busting "server is from the future" reload is in
                // flight — the page goes away; no fallback.
                return;
            }
            let (had_hello, url) = match &a.backend {
                Backend::Remote(r) => (r.connected, r.url.clone()),
                _ => return,
            };
            a.fallback_to_builtin();
            if had_hello {
                log(&format!(
                    "Qwencraft: remote server {url} disconnected — running built-in server"
                ));
                a.set_server_status(
                    "disconnected — running built-in server (re-click Connect to retry)",
                );
            } else {
                log(&format!(
                    "Qwencraft: remote connection to {url} failed — running built-in server"
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
            log("Qwencraft: remote socket error");
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

fn params_from_url() -> (u64, bool, bool, bool, Option<(u32, f32)>, Option<String>, bool, bool) {
    let mut seed = 1337u64;
    let mut verify = false;
    let mut walk = false;
    // `taglog=1` makes the app log name-tag positions every 5 s (headless
    // verification that tags track their players).
    let mut tag_log = false;
    // `dbg=1` logs a verbose chunk-receive/eviction trace (WAN debugging).
    let mut dbg = false;
    // `touchtest=1` forces the mobile touch controls ON (even on fine-
    // pointer headless browsers) and runs a scripted self-test that drives
    // them with real TouchEvents and logs TOUCHTEST telemetry (the mobile
    // end-to-end check, scripts/touch_test.sh).
    let mut touchtest = false;
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
            } else if part.strip_prefix("dbg=").is_some_and(|v| v != "0") {
                dbg = true;
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
                        npcs = Some((c, s.unwrap_or(qwencraft_server::NPC_SPACING_DEFAULT)));
                    }
                }
            } else if let Some(v) = part.strip_prefix("server=") {
                if !v.is_empty() {
                    server = Some(v.to_string());
                }
            } else if part.strip_prefix("touchtest=").is_some_and(|v| v != "0") {
                touchtest = true;
            }
        }
    }
    (seed, verify, walk, tag_log, npcs, server, dbg, touchtest)
}

/// Accept `ws://…`, `wss://…`, or bare `host:port` (scheme implied by the
/// page: `wss` on https pages, `ws` otherwise — a plain `ws://` socket would
/// be blocked as mixed content from an https page).
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
        format!("{}://{s}", default_ws_scheme())
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

/// Scheme implied by the page for a bare `host[:port]` (see
/// `normalize_ws_url`): `wss` when the page is served over https, `ws`
/// otherwise.
fn default_ws_scheme() -> &'static str {
    match web_sys::window().map(|w| w.location().protocol()) {
        Some(Ok(p)) if p == "https:" => "wss",
        _ => "ws",
    }
}

/// Force-reload the page with as much cache busting as a client can do —
/// called when the server speaks a NEWER protocol than this page (the
/// page's assets are stale: the server serves the build matching its own
/// protocol). The reload URL gets a unique query param (existing ones
/// like `?server=` are preserved), so the document URL misses every cache
/// — browser and intermediaries alike — and any registered service worker
/// is unregistered (this app registers none; a user-installed one could
/// still intercept). Returns `false` when the per-tab reload budget
/// (sessionStorage, `protocol::future_reload_budget`) is exhausted: the
/// caller then falls back to the built-in server instead of reloading
/// forever.
fn force_reload_cache_busted() -> bool {
    let Some(window) = web_sys::window() else { return false };
    let Ok(Some(storage)) = window.session_storage() else { return false };
    let now = js_sys::Date::now();
    let prev = storage.get_item(FUTURE_RELOAD_KEY).ok().flatten();
    let Some((count, ts)) =
        qwencraft_server::protocol::future_reload_budget(prev.as_deref(), now)
    else {
        return false; // budget exhausted — don't loop
    };
    // Belt and braces: unregister any service workers (fire and forget —
    // the reload doesn't wait for them). `navigator.serviceWorker` is
    // undefined in insecure contexts, so probe it with Reflect first
    // (same pattern as the `navigator.gpu` check).
    let sw = js_sys::Reflect::get(&window.navigator(), &JsValue::from_str("serviceWorker"))
        .ok()
        .filter(|v| !v.is_undefined());
    if let Some(sw) = sw {
        let sw: web_sys::ServiceWorkerContainer = sw.unchecked_into();
        let regs = sw.get_registrations();
        spawn_local(async move {
            let Ok(regs) = wasm_bindgen_futures::JsFuture::from(regs).await else {
                return
            };
            let Ok(list) = regs.dyn_into::<js_sys::Array>() else { return };
            for r in list.iter() {
                let Ok(reg) = r.dyn_into::<web_sys::ServiceWorkerRegistration>() else {
                    continue;
                };
                let _ = reg.unregister();
            }
        });
    }
    let _ = storage.set_item(FUTURE_RELOAD_KEY, &format!("{count},{ts}"));
    // Unique query param → new document URL → cache miss everywhere.
    let loc = window.location();
    let search = loc.search().ok().unwrap_or_default();
    let sep = if search.is_empty() { "?" } else { "&" };
    let token = format!(
        "{:x}{:x}",
        now as i64,
        (js_sys::Math::random() * 4294967296.0) as u32 // 2^32
    );
    let pathname = loc.pathname().ok().unwrap_or_default();
    let url = format!("{pathname}{sep}qwc_reload={token}");
    let _ = window.location().replace(&url);
    true
}

// World point → screen pixels for the name tags. Lives in
// `qwencraft_world::camera` (next to `view_projection`) so the exact math
// the tags use is host-tested there (this crate is wasm-only).
pub use qwencraft_world::camera::project_point;

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
/// The first touch in a list (our pads track one touch each, so the rest
/// is irrelevant).
fn first_touch(list: TouchList) -> Option<Touch> {
    list.item(0)
}

/// The touch with identifier `id` in a list (changedTouches carries the
/// touch(es) this event is about; the identifier ties a multi-touch
/// gesture to the pad that started it).
fn find_touch(list: TouchList, id: i32) -> Option<Touch> {
    let mut i = 0;
    while i < list.length() {
        if let Some(t) = list.item(i) {
            if t.identifier() == id {
                return Some(t);
            }
        }
        i += 1;
    }
    None
}

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

// ---- Browser console API (window.qwc) ------------------------------------
//
// A small JS API on `window` for inspecting and driving the game from the
// browser console. Golden rule 4 holds: the client never mutates world
// state itself — every write goes through the authoritative server
// (built-in: a direct `Server` call; remote: protocol v6
// `GetBlock`/`SetBlock`/`Teleport` messages), and reads are authoritative
// too (`getBlock` round-trips the server even when this client already
// streams that chunk).

/// A JS Promise with Rust-side handles to settle it later. The
/// `Promise::new` executor runs synchronously and hands over the browser's
/// resolve/reject functions; stashing them lets a later event (the
/// WebSocket `BlockAt` answer to a `qwc.getBlock` request) settle it.
struct JsPromise {
    promise: js_sys::Promise,
    resolve: Function,
    reject: Function,
}

impl JsPromise {
    fn new() -> Self {
        let (mut r, mut j) = (None, None);
        let promise = js_sys::Promise::new(&mut |res, rej| {
            r = Some(res);
            j = Some(rej);
        });
        Self {
            promise,
            resolve: r.expect("the Promise executor must run synchronously"),
            reject: j.expect("the Promise executor must run synchronously"),
        }
    }

    fn resolve(&self, value: JsValue) {
        let _ = self.resolve.call1(&JsValue::NULL, &value);
    }

    fn reject(&self, reason: &str) {
        let _ = self.reject.call1(&JsValue::NULL, &JsValue::from_str(reason));
    }
}

/// A promise already rejected (bad console arguments).
fn rejected_promise(reason: &str) -> JsValue {
    let p = JsPromise::new();
    p.reject(reason);
    p.promise.into()
}

/// Wrap a `Vec<JsValue>`-taking closure in a proper variadic JS function
/// `(...args) => closure(args)`. A bare `into_js_value()` would expose the
/// raw wasm adapter, which expects the arguments as ONE array — calling
/// `qwc.getBlock(1, 2, 3)` from JS would then pass `undefined`. The wrapper
/// also takes ownership of the closure (JS-GC lifetime: the wasm side is
/// reclaimed when `window.qwc` drops the function).
fn to_variadic_js(cb: Closure<dyn FnMut(Vec<JsValue>) -> JsValue>) -> Result<JsValue, JsValue> {
    // factory: (cb) => (...args) => cb(args)
    let make = js_sys::Function::new_no_args("return (cb) => (...args) => cb(args)");
    let make: Function = make.call0(&JsValue::NULL)?.dyn_into()?;
    make.call1(&JsValue::NULL, &cb.into_js_value())
}

/// `(x, y, z)` console arguments as integer block coordinates.
fn parse_xyz_i32(args: &[JsValue]) -> Option<BlockPos> {
    if args.len() < 3 {
        return None;
    }
    let i32_arg = |v: &JsValue| -> Option<i32> {
        let n = v.as_f64()?;
        if !n.is_finite() || n.fract() != 0.0 {
            return None;
        }
        i32::try_from(n as i64).ok()
    };
    Some(BlockPos::new(i32_arg(&args[0])?, i32_arg(&args[1])?, i32_arg(&args[2])?))
}

/// `(x, y, z)` console arguments as floating-point world coordinates
/// (teleports accept fractional positions).
fn parse_xyz_f32(args: &[JsValue]) -> Option<Vec3> {
    if args.len() < 3 {
        return None;
    }
    let f_arg = |v: &JsValue| -> Option<f32> {
        let n = v.as_f64()?;
        n.is_finite().then_some(n as f32)
    };
    Some(Vec3::new(f_arg(&args[0])?, f_arg(&args[1])?, f_arg(&args[2])?))
}

/// The `setBlock` block argument: a registry name (any case, e.g. "stone",
/// "air") or a numeric id (0..=16 — the whole registry, not just the
/// hotbar: the server accepts console edits for every block).
fn parse_block_arg(v: &JsValue) -> Option<Block> {
    if let Some(n) = v.as_f64() {
        if n.is_finite() && n.fract() == 0.0 && (0.0..=16.0).contains(&n) {
            return Some(Block::from_u8(n as u8));
        }
        return None;
    }
    let s = v.as_string()?.trim().to_lowercase();
    BLOCKS
        .iter()
        .find(|b| b.name.to_lowercase() == s)
        .map(|b| Block::from_u8(b.id))
}

/// The JS value for a block read: `{x, y, z, id, name}`.
fn block_obj(pos: BlockPos, block: Block) -> JsValue {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &JsValue::from_str("x"), &JsValue::from_f64(pos.x as f64));
    let _ = js_sys::Reflect::set(&o, &JsValue::from_str("y"), &JsValue::from_f64(pos.y as f64));
    let _ =
        js_sys::Reflect::set(&o, &JsValue::from_str("z"), &JsValue::from_f64(pos.z as f64));
    let _ = js_sys::Reflect::set(
        &o,
        &JsValue::from_str("id"),
        &JsValue::from_f64(block.as_u8() as f64),
    );
    let _ = js_sys::Reflect::set(
        &o,
        &JsValue::from_str("name"),
        &JsValue::from_str(block.info().name),
    );
    o.into()
}

/// The console usage help (logged on startup and by `qwc.help()`).
/// `?touchtest=1` self-test: a classic script injected into the page that
/// drives the touch controls with real `TouchEvent`s and logs `TOUCHTEST`
/// telemetry (`scripts/touch_test.sh` greps for it). It is a black box —
/// DOM + `window.qwc` only, the same surface a real user has: tap-to-play,
/// look-pad drag, joystick walk, JUMP hold, BREAK/PLACE on the crosshair,
/// and a hotbar slot tap (the placed block must be the tapped one).
// Note: r##"…"## — the JS below contains "# (querySelectorAll("#hotbar…")),
// which would terminate a plain r"#" raw string.
const TOUCHTEST_JS: &str = r##"
(function () {
  "use strict";
  function log(m) { console.log("TOUCHTEST " + m); }
  function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }
  // fail() just throws; the catch below logs "TOUCHTEST FAIL …" once.
  function fail(m) { throw new Error(m); }

  var nextId = 1;
  function mkTouch(el, x, y, id) {
    return new Touch({ identifier: id, target: el, clientX: x, clientY: y,
                       pageX: x, pageY: y, radiusX: 3, radiusY: 3, force: 1 });
  }
  function fire(el, type, touches, changed) {
    var ev = new TouchEvent(type, {
      touches: touches, targetTouches: touches, changedTouches: changed,
      bubbles: true, cancelable: true, composed: true
    });
    el.dispatchEvent(ev);
    return ev.defaultPrevented;
  }
  function tap(el, id) {
    var r = el.getBoundingClientRect();
    var x = r.left + r.width / 2, y = r.top + r.height / 2;
    var t = mkTouch(el, x, y, id);
    // A real (trusted) tap whose touchstart was NOT preventDefaulted makes
    // the browser fire a synthetic click afterwards — untrusted synthetic
    // touches never do, so we replay it here (that is the path the hotbar
    // slot selection rides on).
    var prevented = fire(el, "touchstart", [t], [t]);
    fire(el, "touchend", [], [t]);
    if (!prevented) {
      el.dispatchEvent(new MouseEvent("click", {
        bubbles: true, cancelable: true, detail: 1, view: window,
        clientX: x, clientY: y
      }));
    }
  }
  function waitForTarget() {
    return new Promise(function (resolve) {
      var tries = 0;
      (function poll() {
        var t = window.qwc.getPlayer().target;
        if (t || tries++ > 40) resolve(t || null);
        else setTimeout(poll, 100);
      })();
    });
  }
  // The place cell is the target plus one face normal → one of the six
  // neighbour cells. Resolve with its coordinates, or null.
  function scanNeighbours(t, id) {
    var dirs = [[1,0,0],[-1,0,0],[0,1,0],[0,-1,0],[0,0,1],[0,0,-1]];
    var i = 0;
    function next() {
      if (i >= dirs.length) return Promise.resolve(null);
      var d = dirs[i++];
      return window.qwc.getBlock(t.x + d[0], t.y + d[1], t.z + d[2]).then(function (b) {
        return b.id === id ? "(" + b.x + "," + b.y + "," + b.z + ")" : next();
      });
    }
    return next();
  }

  (async function () {
    try {
      // Wait for the app: qwc + a live player (pos.y > 1 means the built-in
      // server has spawned and ticked).
      var p = null;
      for (var i = 0; i < 300; i++) {
        p = window.qwc ? window.qwc.getPlayer() : null;
        if (p && p.y > 1) break;
        await sleep(100);
      }
      if (!p || p.y <= 1) fail("no live player state (app not running?)");

      // 1) TAP TO PLAY: the overlay's touchstart must hide the overlay.
      var ov = document.getElementById("overlay");
      var ovT = mkTouch(ov, innerWidth / 2, innerHeight / 2, nextId++);
      fire(ov, "touchstart", [ovT], [ovT]);
      await sleep(200);
      if (getComputedStyle(ov).display !== "none") fail("overlay still visible after tap");
      var ui = document.getElementById("touch-ui");
      if (getComputedStyle(ui).display === "none") fail("touch UI not shown");
      log("start ok (overlay dismissed, pads shown)");

      // 2) LOOK pad: drag right (yaw), then drag down (pitch — and aim at
      //    the ground so the break/place tests have a target).
      var lp = document.getElementById("lookpad");
      var lx = Math.round(innerWidth * 0.75), ly = Math.round(innerHeight * 0.35);
      var yaw0 = window.qwc.getPlayer().yaw;
      var pitch0 = window.qwc.getPlayer().pitch;
      var id = nextId++;
      fire(lp, "touchstart", [mkTouch(lp, lx, ly, id)], [mkTouch(lp, lx, ly, id)]);
      // One continuous drag (no jumps back — a jump would cancel itself
      // out: dx is a delta). Right for the yaw, then down for the pitch.
      var mx = lx, my = ly;
      for (var k = 1; k <= 5; k++) {
        await sleep(60);
        mx += 18;
        fire(lp, "touchmove", [mkTouch(lp, mx, my, id)], [mkTouch(lp, mx, my, id)]);
      }
      for (var k = 1; k <= 8; k++) {
        await sleep(60);
        my += 25;
        fire(lp, "touchmove", [mkTouch(lp, mx, my, id)], [mkTouch(lp, mx, my, id)]);
      }
      fire(lp, "touchend", [], [mkTouch(lp, mx, my, id)]);
      await sleep(400);
      var p1 = window.qwc.getPlayer();
      var dyaw = yaw0 - p1.yaw;      // drag right → yaw -= dx*sens
      var dpitch = pitch0 - p1.pitch; // drag down → pitch -= dy*sens (looks down)
      if (Math.abs(dyaw) < 0.03) fail("look: yaw barely changed (" + dyaw.toFixed(3) + ")");
      if (dpitch < 0.1) fail("look: pitch did not look down (" + dpitch.toFixed(3) + ")");
      log("look ok (dyaw=" + dyaw.toFixed(3) + " dpitch=" + dpitch.toFixed(3) + ")");

      // 3) MOVE pad: hold the stick half out for ~1.3 s; if the terrain
      //    blocks that way (spawned against a tree/wall), retry backwards.
      var joy = document.getElementById("joy");
      var jr = joy.getBoundingClientRect();
      var jx = jr.left + jr.width / 2, jy = jr.top + jr.height / 2;
      async function walkWithStick(sx, sy) {
        var jid = nextId++;
        fire(joy, "touchstart", [mkTouch(joy, jx, jy, jid)], [mkTouch(joy, jx, jy, jid)]);
        await sleep(100);
        fire(joy, "touchmove", [mkTouch(joy, jx + sx * 20, jy - sy * 20, jid)],
                            [mkTouch(joy, jx + sx * 20, jy - sy * 20, jid)]);
        await sleep(100);
        var a = window.qwc.getPlayer();
        await sleep(1300);
        var b = window.qwc.getPlayer();
        fire(joy, "touchend", [], [mkTouch(joy, jx + sx * 20, jy - sy * 20, jid)]);
        await sleep(100);
        return Math.hypot(b.x - a.x, b.z - a.z);
      }
      var md = await walkWithStick(0, 1);
      if (md < 1.0) md = await walkWithStick(0, -1);
      if (md < 1.0) fail("move: player did not walk either way (dist=" + md.toFixed(2) + ")");
      if (md > 8.0) fail("move: walked " + md.toFixed(2) + " m in 1.3 s (throttle lost?)");
      log("move ok (dist=" + md.toFixed(2) + " m, half stick over ~1.3 s)");

      // 4) JUMP button: hold it until the player leaves the ground.
      var jb = document.getElementById("btn-jump");
      for (var i = 0; i < 50 && !window.qwc.getPlayer().onGround; i++) await sleep(100);
      var jbr = jb.getBoundingClientRect();
      var jid2 = nextId++;
      var jbt = mkTouch(jb, jbr.left + 10, jbr.top + 10, jid2);
      fire(jb, "touchstart", [jbt], [jbt]);
      var airborne = false;
      for (var i = 0; i < 15; i++) {
        await sleep(80);
        if (!window.qwc.getPlayer().onGround) { airborne = true; break; }
      }
      fire(jb, "touchend", [], [jbt]);
      if (!airborne) fail("jump: player never left the ground");
      log("jump ok (left the ground while holding JUMP)");

      // 5) BREAK button: break the crosshair target (the ground, after the
      //    look-down in step 2).
      var tgt = await waitForTarget();
      if (!tgt) fail("break: no crosshair target");
      var before = await window.qwc.getBlock(tgt.x, tgt.y, tgt.z);
      tap(document.getElementById("btn-break"), nextId++);
      await sleep(500);
      var after = await window.qwc.getBlock(tgt.x, tgt.y, tgt.z);
      if (before.id === after.id) fail("break: block unchanged (" + before.name + ")");
      if (after.id !== 0) fail("break: block is now " + after.name + " (expected air)");
      log("break ok (" + before.name + " -> air)");

      // 6) PLACE button: places the hotbar-selected block against the (new)
      //    target's face → it appears in one of the target's six neighbours.
      var blocks = window.qwc.listBlocks();
      var placeables = blocks.filter(function (b) { return b.placeable; })
                             .sort(function (a, b) { return a.id - b.id; });
      var slot0 = placeables[0]; // hotbar slot 1
      var t2 = await waitForTarget();
      if (!t2) fail("place: no crosshair target");
      tap(document.getElementById("btn-place"), nextId++);
      await sleep(500);
      var found = await scanNeighbours(t2, slot0.id);
      if (!found) fail("place: " + slot0.name + " not found around the target");
      log("place ok (" + slot0.name + " at " + found + ")");

      // 7) HOTBAR tap: select slot 3 (Stone), place again, find the stone.
      var slots = document.querySelectorAll("#hotbar .hotbar-slot");
      if (slots.length < 3) fail("hotbar: fewer than 3 slots");
      tap(slots[2], nextId++);
      await sleep(200);
      var stone = null;
      for (var i = 0; i < 9 && !stone; i++) {
        if (placeables[i] && placeables[i].id === 3) stone = placeables[i];
      }
      if (!stone) stone = placeables[2];
      var t3 = await waitForTarget();
      if (!t3) fail("hotbar: no crosshair target");
      tap(document.getElementById("btn-place"), nextId++);
      await sleep(500);
      var found2 = await scanNeighbours(t3, stone.id);
      if (!found2) fail("hotbar: " + stone.name + " not found (slot tap ignored?)");
      log("hotbar ok (tapped slot 3 -> " + stone.name + " at " + found2 + ")");

      log("ALL OK (start, look, move, jump, break, place, hotbar)");
    } catch (err) {
      log("FAIL " + (err && err.message ? err.message : String(err)));
    }
  })();
})();
"##;

/// Inject the touch self-test script (runs synchronously on append).
fn inject_touchtest_script(document: &web_sys::Document) {
    let el: HtmlElement = document
        .create_element("script")
        .expect("script")
        .dyn_into()
        .expect("script");
    el.set_text_content(Some(TOUCHTEST_JS));
    if let Some(body) = document.body() {
        let _ = body.append_child(&el);
    }
}

fn console_greeting() -> &'static str {
    "Qwencraft console API — window.qwc:
  qwc.getBlock(x, y, z)        → Promise<{x, y, z, id, name}>
  qwc.setBlock(x, y, z, block) → Promise (block: a name like \"stone\" or an id; \"air\" breaks)
  qwc.getPlayer()              → {x, y, z, yaw, pitch, onGround, fly, flySpeed, name, target}
  qwc.setPlayerPos(x, y, z)    → Promise (teleport; y is the feet height)
  qwc.listBlocks()             → [{id, name, placeable, solid, water}, …]
  qwc.help()                   → show this help again"
}

/// Install the browser console API on `window.qwc` and log the usage
/// greeting. All writes go through the authoritative server and all reads
/// are authoritative (see the section notes above).
fn install_console_api(app: &Rc<RefCell<App>>) -> Result<(), JsValue> {
    // qwc.getBlock(x, y, z) → Promise<{x, y, z, id, name}>.
    // Built-in: answered synchronously from the embedded world. Remote:
    // a round-trip — the promise is parked in `pending_blocks` and settled
    // when the `BlockAt` answer arrives (or rejected when the link dies).
    let get_block = {
        let app = app.clone();
        Closure::<dyn FnMut(Vec<JsValue>) -> JsValue>::new(
            move |args: Vec<JsValue>| -> JsValue {
                let Some(pos) = parse_xyz_i32(&args) else {
                    return rejected_promise(
                        "getBlock(x, y, z) — three integer coordinates expected",
                    );
                };
            let mut a = app.borrow_mut();
            let p = JsPromise::new();
            match a.backend.console_get_block(pos) {
                ConsoleGetBlock::Answered(block) => p.resolve(block_obj(pos, block)),
                ConsoleGetBlock::RequestSent => {
                    let link = a.remote_id();
                    let pending: JsValue = p.promise.clone().into();
                    a.pending_blocks.push((link, pos, p));
                    return pending;
                }
                ConsoleGetBlock::NotConnected => {
                    p.reject("not connected to a server")
                }
            }
            p.promise.into()
            },
        )
    };

    // qwc.setBlock(x, y, z, block) → Promise<{x, y, z, id, name}>.
    // Resolves once the server has applied the edit (built-in: immediately;
    // remote: when the message is queued — the world update then arrives
    // via the normal chunk stream).
    let set_block = {
        let app = app.clone();
        Closure::<dyn FnMut(Vec<JsValue>) -> JsValue>::new(
            move |args: Vec<JsValue>| -> JsValue {
                let Some(pos) = parse_xyz_i32(&args) else {
                    return rejected_promise(
                        "setBlock(x, y, z, block) — three integer coordinates + a block expected",
                    );
                };
            let Some(block) = args.get(3).and_then(parse_block_arg) else {
                return rejected_promise(
                    "setBlock: block must be a name (\"stone\", \"air\", …) or an id (0-16) — see qwc.listBlocks()",
                );
            };
            let mut a = app.borrow_mut();
            let p = JsPromise::new();
            match a.backend.console_set_block(pos, block) {
                Ok(()) => p.resolve(block_obj(pos, block)),
                Err(e) => p.reject(&e),
            }
            p.promise.into()
            },
        )
    };

    // qwc.getPlayer() → the latest player state (synchronous: the client
    // already holds it — the camera source of truth).
    let get_player = {
        let app = app.clone();
        Closure::<dyn FnMut(Vec<JsValue>) -> JsValue>::new(
            move |_args: Vec<JsValue>| -> JsValue {
                let a = app.borrow();
            let p = a.backend.player_state();
            let o = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("x"),
                &JsValue::from_f64(p.pos.x as f64),
            );
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("y"),
                &JsValue::from_f64(p.pos.y as f64),
            );
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("z"),
                &JsValue::from_f64(p.pos.z as f64),
            );
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("yaw"),
                &JsValue::from_f64(p.yaw as f64),
            );
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("pitch"),
                &JsValue::from_f64(p.pitch as f64),
            );
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("onGround"),
                &JsValue::from_bool(p.on_ground),
            );
            let _ =
                js_sys::Reflect::set(&o, &JsValue::from_str("fly"), &JsValue::from_bool(p.fly));
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("flySpeed"),
                &JsValue::from_f64(p.fly_speed as f64),
            );
            let _ = js_sys::Reflect::set(
                &o,
                &JsValue::from_str("name"),
                &JsValue::from_str(&p.name),
            );
            // The block under the crosshair (null when nothing in range) —
            // the same target the break/place buttons (and clicks) act on.
            let target = match p.target {
                Some(t) => {
                    let o = js_sys::Object::new();
                    let _ = js_sys::Reflect::set(
                        &o,
                        &JsValue::from_str("x"),
                        &JsValue::from_f64(t.x as f64),
                    );
                    let _ = js_sys::Reflect::set(
                        &o,
                        &JsValue::from_str("y"),
                        &JsValue::from_f64(t.y as f64),
                    );
                    let _ = js_sys::Reflect::set(
                        &o,
                        &JsValue::from_str("z"),
                        &JsValue::from_f64(t.z as f64),
                    );
                    o.into()
                }
                None => JsValue::NULL,
            };
            let _ = js_sys::Reflect::set(&o, &JsValue::from_str("target"), &target);
            o.into()
            },
        )
    };

    // qwc.setPlayerPos(x, y, z) → Promise (teleport; the server clamps y
    // into the world and the next PlayerState carries the new position).
    let set_player_pos = {
        let app = app.clone();
        Closure::<dyn FnMut(Vec<JsValue>) -> JsValue>::new(
            move |args: Vec<JsValue>| -> JsValue {
                let Some(pos) = parse_xyz_f32(&args) else {
                    return rejected_promise(
                        "setPlayerPos(x, y, z) — three numbers expected (y is the feet height)",
                    );
                };
            let mut a = app.borrow_mut();
            let p = JsPromise::new();
            match a.backend.console_teleport(pos) {
                Ok(()) => {
                    let o = js_sys::Object::new();
                    let _ = js_sys::Reflect::set(&o, &JsValue::from_str("ok"), &JsValue::from_bool(true));
                    let _ = js_sys::Reflect::set(
                        &o,
                        &JsValue::from_str("x"),
                        &JsValue::from_f64(pos.x as f64),
                    );
                    let _ = js_sys::Reflect::set(
                        &o,
                        &JsValue::from_str("y"),
                        &JsValue::from_f64(pos.y as f64),
                    );
                    let _ = js_sys::Reflect::set(
                        &o,
                        &JsValue::from_str("z"),
                        &JsValue::from_f64(pos.z as f64),
                    );
                    p.resolve(o.into());
                }
                Err(e) => p.reject(&e),
            }
            p.promise.into()
            },
        )
    };

    // qwc.listBlocks() → the whole registry (id, name, physics flags).
    let list_blocks =
        Closure::<dyn FnMut(Vec<JsValue>) -> JsValue>::new(
            move |_args: Vec<JsValue>| -> JsValue {
                let arr = js_sys::Array::new();
            for b in BLOCKS.iter() {
                let o = js_sys::Object::new();
                let _ = js_sys::Reflect::set(
                    &o,
                    &JsValue::from_str("id"),
                    &JsValue::from_f64(b.id as f64),
                );
                let _ = js_sys::Reflect::set(
                    &o,
                    &JsValue::from_str("name"),
                    &JsValue::from_str(b.name),
                );
                let _ = js_sys::Reflect::set(
                    &o,
                    &JsValue::from_str("placeable"),
                    &JsValue::from_bool(b.placeable),
                );
                let _ =
                    js_sys::Reflect::set(&o, &JsValue::from_str("solid"), &JsValue::from_bool(b.solid));
                let _ =
                    js_sys::Reflect::set(&o, &JsValue::from_str("water"), &JsValue::from_bool(b.water));
                let _ = arr.push(&o.into());
            }
            arr.into()
            },
        );

    // qwc.help() → log the usage again.
    let help = Closure::<dyn FnMut(Vec<JsValue>) -> JsValue>::new(
        move |_args: Vec<JsValue>| -> JsValue {
            log(console_greeting());
            JsValue::UNDEFINED
        },
    );

    let api = js_sys::Object::new();
    // Each closure is wrapped into a variadic JS function and handed to the
    // JS GC (see `to_variadic_js`): the functions live on `window.qwc` for
    // the life of the page, and the wasm side is reclaimed when JS drops
    // them.
    js_sys::Reflect::set(
        &api,
        &JsValue::from_str("getBlock"),
        &to_variadic_js(get_block)?,
    )?;
    js_sys::Reflect::set(
        &api,
        &JsValue::from_str("setBlock"),
        &to_variadic_js(set_block)?,
    )?;
    js_sys::Reflect::set(
        &api,
        &JsValue::from_str("getPlayer"),
        &to_variadic_js(get_player)?,
    )?;
    js_sys::Reflect::set(
        &api,
        &JsValue::from_str("setPlayerPos"),
        &to_variadic_js(set_player_pos)?,
    )?;
    js_sys::Reflect::set(
        &api,
        &JsValue::from_str("listBlocks"),
        &to_variadic_js(list_blocks)?,
    )?;
    js_sys::Reflect::set(&api, &JsValue::from_str("help"), &to_variadic_js(help)?)?;

    let window: Window = web_sys::window().expect("no window");
    js_sys::Reflect::set(&JsValue::from(window), &JsValue::from_str("qwc"), &api)?;

    log(console_greeting());
    Ok(())
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

    let (seed, verify_mode, walk_mode, tag_log, npcs, server_url_param, dbg, touchtest) =
        params_from_url();
    log(&format!("Qwencraft: app started (seed {seed})"));

    // Touch (mobile) mode: a coarse pointer means the two thumb pads
    // replace pointer lock (pointer lock is unusable from touch — iOS
    // Safari doesn't even have it). `?touchtest=1` forces it on for the
    // headless self-test (scripts/touch_test.sh).
    let touch_mode = touchtest
        || window
            .match_media("(pointer: coarse)")
            .ok()
            .flatten()
            .is_some_and(|m| m.matches());

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
    let hotbar = document
        .get_element_by_id("hotbar")
        .and_then(|e| e.dyn_into::<HtmlDivElement>().ok())
        .expect("missing #hotbar");
    let hotbar_name = document
        .get_element_by_id("hotbar-name")
        .and_then(|e| e.dyn_into::<HtmlDivElement>().ok())
        .expect("missing #hotbar-name");

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

    // Touch mode: switch the UI to its mobile layout (body class shows the
    // pads and the touch help lines, hides the keyboard/mouse ones) and
    // relabel the start prompt.
    if touch_mode {
        if let Some(body) = document.body() {
            let _ = body.class_list().add_1("touch");
        }
        if let Some(play) = document
            .get_element_by_id("play")
            .and_then(|e| e.dyn_into::<HtmlElement>().ok())
        {
            play.set_text_content(Some("TAP ANYWHERE TO PLAY"));
        }
        log("Qwencraft: touch controls enabled (coarse pointer or ?touchtest=1)");
    }

    let app = Rc::new(RefCell::new(App::new(
        seed,
        hud.clone(),
        overlay.clone(),
        server_status.clone(),
        tags.clone(),
        hotbar.clone(),
        hotbar_name.clone(),
        verify_mode,
        walk_mode,
        tag_log,
        dbg,
        npcs,
    )));
    // The event handlers below gate everything on App::touch_mode.
    app.borrow_mut().touch_mode = touch_mode;
    // Build the hotbar slots (block list comes from the shared registry,
    // so a new block appears here automatically once it is placeable).
    app.borrow_mut().build_hotbar();
    // Browser console API (window.qwc) + the usage greeting in the console.
    install_console_api(&app)?;
    if let Some((c, s)) = npcs {
        log(&format!("Qwencraft: NPC load test armed: {c} agents @ {s:.0} m spacing"));
    }
    // Server-connect panel: pre-fill from ?server= and connect right away
    // (the headless-test path); otherwise the user drives it from the UI.
    if let Some(url) = server_url_param.as_deref() {
        server_input.set_value(url);
        log(&format!("Qwencraft: auto-connecting to {url}"));
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
                // Hotbar: digit keys 1-9 select a slot (one-shot; repeats
                // from key auto-repeat are harmless but needlessly touch
                // the DOM).
                if !e.repeat() {
                    if let Some(d) = e.code().as_str().strip_prefix("Digit") {
                        if let Ok(d) = d.parse::<usize>() {
                            if d >= 1 && d <= HOTBAR_SLOTS {
                                e.prevent_default();
                                app.borrow_mut().select_slot(d - 1);
                                return;
                            }
                        }
                    }
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
                // Right-click places the hotbar-selected block; the
                // server validates the id.
                let block = a.selected_block().as_u8();
                match e.button() {
                    0 => a.actions.push(Action::Break { yaw, pitch }),
                    2 => a.actions.push(Action::Place { yaw, pitch, block }),
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

        // Mouse wheel: cycle the hotbar selection (forward = next slot).
        // Works whether or not the pointer is locked; ignored while
        // typing in a text field (the options panel).
        let cb = Closure::<dyn FnMut(WheelEvent)>::new({
            let app = app.clone();
            move |e: WheelEvent| {
                let in_text = e
                    .target()
                    .and_then(|t| t.dyn_ref::<HtmlElement>().cloned())
                    .is_some_and(|t| matches!(t.tag_name().as_str(), "INPUT" | "TEXTAREA"));
                if in_text {
                    return;
                }
                let dir = if e.delta_y() > 0.0 { 1 } else { -1 };
                let mut a = app.borrow_mut();
                let cur = a.selected_slot;
                let next = if dir > 0 {
                    (cur + 1) % HOTBAR_SLOTS
                } else {
                    (cur + HOTBAR_SLOTS - 1) % HOTBAR_SLOTS
                };
                a.select_slot(next);
            }
        });
        window
            .add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref())
            .expect("wheel listener");
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
                // Touch devices can't usefully pointer-lock; taps start the
                // game via the overlay's touchstart handler instead.
                if a.touch_mode || a.locked {
                    return;
                }
                let _ = canvas_for_lock.request_pointer_lock();
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
                // Touch mode never pointer-locks — its overlay state is
                // driven by taps (the overlay touchstart / the menu
                // button), so a stray change event must not re-show it.
                if app.borrow().touch_mode {
                    return;
                }
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

        // ---- Touch controls: two thumb pads (mobile) ---------------------
        // Shown only in touch mode (coarse pointer / ?touchtest=1). The
        // LEFT pad is an analog joystick: its stick vector is sent as the
        // analog part of the per-frame Input (the server scales walk speed
        // by it — stick distance from centre is the throttle). The RIGHT
        // pad drags the view: screen pixels → the same dx/dy a mouse would
        // send (the server converts at the mouse sensitivity). BREAK/PLACE
        // act on the crosshair with the aim of the last rendered frame
        // (same aim semantics as clicks); JUMP is level-triggered like
        // holding Space (re-jumps on landing); FLY toggles fly mode.
        // Every pad tracks ONE touch identifier, so both thumbs + a button
        // work simultaneously (multi-touch).
        if touch_mode {
            // Tap-to-play: a tap outside the options panel starts the game
            // (no pointer lock on touch). preventDefault kills the synthetic
            // click so the desktop pointer-lock path can't fire.
            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let overlay_ref = overlay.clone();
                let options_ref = options.clone();
                move |e: TouchEvent| {
                    let a = app.borrow();
                    if !a.touch_mode || a.locked {
                        return;
                    }
                    drop(a);
                    // Options taps must reach their own (synthetic) clicks.
                    if e
                        .target()
                        .and_then(|t| t.dyn_ref::<Element>().cloned())
                        .is_some_and(|t| is_inside(&t, &options_ref))
                    {
                        return;
                    }
                    e.prevent_default();
                    let mut a = app.borrow_mut();
                    a.locked = true;
                    a.keys = KeySet::default();
                    a.joy_x = 0.0;
                    a.joy_y = 0.0;
                    let _ = overlay_ref.style().set_property("display", "none");
                    log("Qwencraft: touch controls active (tap-to-play)");
                }
            });
            overlay
                .add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())
                .expect("overlay touchstart listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            // LEFT PAD: analog joystick (the move stick).
            let joy = document
                .get_element_by_id("joy")
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                .expect("missing #joy");
            let joy_knob = document
                .get_element_by_id("joy-knob")
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                .expect("missing #joy-knob");
            let joy_id: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));

            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let id_ref = joy_id.clone();
                move |e: TouchEvent| {
                    let a = app.borrow();
                    if !a.touch_mode || id_ref.borrow().is_some() {
                        return;
                    }
                    let Some(t) = first_touch(e.changed_touches()) else {
                        return;
                    };
                    *id_ref.borrow_mut() = Some(t.identifier());
                    e.prevent_default();
                }
            });
            joy
                .add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())
                .expect("joy touchstart listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let id_ref = joy_id.clone();
                let joy_ref = joy.clone();
                let knob_ref = joy_knob.clone();
                move |e: TouchEvent| {
                    let Some(id) = *id_ref.borrow() else {
                        return;
                    };
                    let Some(t) = find_touch(e.changed_touches(), id) else {
                        return;
                    };
                    e.prevent_default();
                    let rect = joy_ref.get_bounding_client_rect();
                    let cx = rect.x() + rect.width() / 2.0;
                    let cy = rect.y() + rect.height() / 2.0;
                    // Knob travel: pad radius minus the knob's radius.
                    let travel = (rect.width() / 2.0 - 36.0).max(20.0) as f32;
                    let mut dx = (t.client_x() as f64 - cx) as f32;
                    let mut dy = (t.client_y() as f64 - cy) as f32;
                    let len = (dx * dx + dy * dy).sqrt();
                    if len > travel {
                        dx *= travel / len;
                        dy *= travel / len;
                    }
                    let _ = knob_ref.style().set_property(
                        "transform",
                        &format!("translate({dx:.1}px, {dy:.1}px)"),
                    );
                    // Stick vector: screen-right = +x, screen-up = forward
                    // (+y). Deadzone: a resting thumb jitters a few pixels
                    // — below 15% of travel the stick reads centred (the
                    // server would otherwise creep at 3% speed).
                    let (sx, sy) = (dx / travel, -dy / travel);
                    let mut a = app.borrow_mut();
                    if (sx * sx + sy * sy).sqrt() < 0.15 {
                        a.joy_x = 0.0;
                        a.joy_y = 0.0;
                    } else {
                        a.joy_x = sx;
                        a.joy_y = sy;
                    }
                }
            });
            joy
                .add_event_listener_with_callback("touchmove", cb.as_ref().unchecked_ref())
                .expect("joy touchmove listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            // Release (touchend AND touchcancel — the browser fires both on
            // the element the touch STARTED on, even if the thumb left it,
            // and touchcancel for interruptions like incoming calls).
            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let id_ref = joy_id.clone();
                let knob_ref = joy_knob.clone();
                move |e: TouchEvent| {
                    let Some(id) = *id_ref.borrow() else {
                        return;
                    };
                    let Some(_) = find_touch(e.changed_touches(), id) else {
                        return;
                    };
                    *id_ref.borrow_mut() = None;
                    let _ = knob_ref.style().set_property("transform", "translate(0px, 0px)");
                    let mut a = app.borrow_mut();
                    a.joy_x = 0.0;
                    a.joy_y = 0.0;
                    e.prevent_default();
                }
            });
            for name in ["touchend", "touchcancel"] {
                joy
                    .add_event_listener_with_callback(name, cb.as_ref().unchecked_ref())
                    .expect("joy touchend listener");
            }
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            // RIGHT PAD: drag to look. Full-screen (under the controls, which
            // hit-test first): every non-control touch is a look drag.
            // Screen pixels → the same dx/dy a mouse would send (the server
            // converts at the mouse sensitivity, 0.0024 rad/px).
            let lookpad = document
                .get_element_by_id("lookpad")
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                .expect("missing #lookpad");
            let look_id: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));
            let look_last: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));

            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let id_ref = look_id.clone();
                let last_ref = look_last.clone();
                move |e: TouchEvent| {
                    let a = app.borrow();
                    if !a.touch_mode || id_ref.borrow().is_some() {
                        return;
                    }
                    let Some(t) = first_touch(e.changed_touches()) else {
                        return;
                    };
                    *id_ref.borrow_mut() = Some(t.identifier());
                    *last_ref.borrow_mut() = Some((t.client_x() as f64, t.client_y() as f64));
                    e.prevent_default();
                }
            });
            lookpad
                .add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())
                .expect("lookpad touchstart listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let id_ref = look_id.clone();
                let last_ref = look_last.clone();
                move |e: TouchEvent| {
                    let Some(id) = *id_ref.borrow() else {
                        return;
                    };
                    let Some(t) = find_touch(e.changed_touches(), id) else {
                        return;
                    };
                    e.prevent_default();
                    let mut a = app.borrow_mut();
                    if let Some((lx, ly)) = *last_ref.borrow() {
                        a.mouse_dx += (t.client_x() as f64 - lx) as f32;
                        a.mouse_dy += (t.client_y() as f64 - ly) as f32;
                    }
                    *last_ref.borrow_mut() = Some((t.client_x() as f64, t.client_y() as f64));
                }
            });
            lookpad
                .add_event_listener_with_callback("touchmove", cb.as_ref().unchecked_ref())
                .expect("lookpad touchmove listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let id_ref = look_id.clone();
                let last_ref = look_last.clone();
                move |e: TouchEvent| {
                    let Some(id) = *id_ref.borrow() else {
                        return;
                    };
                    let Some(_) = find_touch(e.changed_touches(), id) else {
                        return;
                    };
                    *id_ref.borrow_mut() = None;
                    *last_ref.borrow_mut() = None;
                    e.prevent_default();
                }
            });
            for name in ["touchend", "touchcancel"] {
                lookpad
                    .add_event_listener_with_callback(name, cb.as_ref().unchecked_ref())
                    .expect("lookpad touchend listener");
            }
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            // Action buttons (right thumb, above the hotbar): JUMP is
            // level-triggered (hold = keep Space down, re-jumps on landing,
            // exactly like the keyboard); the others are one-shot on
            // touchstart (instant response — no waiting for touchend/click).
            #[derive(Clone, Copy)]
            enum TouchBtn {
                Jump,
                Fly,
                Break,
                Place,
            }
            for (id, kind) in [
                ("btn-jump", TouchBtn::Jump),
                ("btn-fly", TouchBtn::Fly),
                ("btn-break", TouchBtn::Break),
                ("btn-place", TouchBtn::Place),
            ] {
                let el = document
                    .get_element_by_id(id)
                    .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                    .unwrap_or_else(|| panic!("missing #{id}"));
                let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                    let app = app.clone();
                    let el_ref = el.clone();
                    move |e: TouchEvent| {
                        let mut a = app.borrow_mut();
                        if !a.touch_mode {
                            return;
                        }
                        e.prevent_default();
                        // Stamp the aim of the last rendered frame (same
                        // semantics as clicks) and the selected block
                        // BEFORE touching the action queue (borrow checker).
                        let (yaw, pitch) = (a.aim_yaw, a.aim_pitch);
                        let block = a.selected_block().as_u8();
                        match kind {
                            TouchBtn::Jump => a.keys.insert(Key::Space),
                            TouchBtn::Fly => a.actions.push(Action::ToggleFly),
                            TouchBtn::Break => a.actions.push(Action::Break {
                                yaw,
                                pitch,
                            }),
                            TouchBtn::Place => a.actions.push(Action::Place {
                                yaw,
                                pitch,
                                block,
                            }),
                        }
                        let _ = el_ref.class_list().add_1("pressed");
                    }
                });
                el.add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())
                    .expect("touch button touchstart listener");
                app.borrow_mut()
                    ._closures
                    .push(Box::into_raw(Box::new(cb)) as *mut _);
                let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                    let app = app.clone();
                    let el_ref = el.clone();
                    move |e: TouchEvent| {
                        e.prevent_default();
                        let _ = el_ref.class_list().remove_1("pressed");
                        let mut a = app.borrow_mut();
                        if a.touch_mode && matches!(kind, TouchBtn::Jump) {
                            a.keys.remove(Key::Space);
                        }
                    }
                });
                for name in ["touchend", "touchcancel"] {
                    el.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref())
                        .expect("touch button touchend listener");
                }
                app.borrow_mut()
                    ._closures
                    .push(Box::into_raw(Box::new(cb)) as *mut _);
            }

            // Menu button: re-show the start/options overlay (the mobile
            // equivalent of releasing the pointer lock with Esc).
            let cb = Closure::<dyn FnMut(TouchEvent)>::new({
                let app = app.clone();
                let overlay_ref = overlay.clone();
                move |e: TouchEvent| {
                    let mut a = app.borrow_mut();
                    if !a.touch_mode {
                        return;
                    }
                    e.prevent_default();
                    a.locked = false;
                    a.keys = KeySet::default();
                    a.joy_x = 0.0;
                    a.joy_y = 0.0;
                    let _ = overlay_ref.style().set_property("display", "block");
                }
            });
            if let Some(menu) = document
                .get_element_by_id("btn-menu")
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
            {
                menu
                    .add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())
                    .expect("menu touchstart listener");
            }
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);

            // Hotbar: tappable slots (the click listener also serves desktop
            // mouse clicks — the slots are now pointer-events:auto).
            let cb = Closure::<dyn FnMut(MouseEvent)>::new({
                let app = app.clone();
                move |e: MouseEvent| {
                    let Some(slot) = e
                        .target()
                        .and_then(|t| t.dyn_ref::<Element>().cloned())
                        .and_then(|t| t.closest(".hotbar-slot").ok().flatten())
                    else {
                        return;
                    };
                    let Some(s) = slot.get_attribute("data-slot") else {
                        return;
                    };
                    let Ok(i) = s.parse::<usize>() else {
                        return;
                    };
                    e.prevent_default();
                    app.borrow_mut().select_slot(i);
                }
            });
            hotbar
                .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                .expect("hotbar click listener");
            app.borrow_mut()
                ._closures
                .push(Box::into_raw(Box::new(cb)) as *mut _);
        }

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

    // ?touchtest=1: run the scripted touch self-test (real TouchEvents →
    // the same handlers a thumb would drive; TOUCHTEST telemetry on the
    // console for scripts/touch_test.sh to grep).
    if touchtest {
        inject_touchtest_script(&document);
    }

    // ---- Async renderer init, then main loop ------------------------------
    let app_render = app.clone();
    let canvas_render = canvas.clone();
    spawn_local(async move {
        match Renderer::new(&canvas_render).await {
            Ok(renderer) => {
                app_render.borrow_mut().renderer = Some(renderer);
                log("Qwencraft: renderer ready");
            }
            Err(e) => {
                log(&format!("Qwencraft: renderer init failed: {e}"));
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
