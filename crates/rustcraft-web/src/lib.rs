//! RustCraft browser entry point.
//!
//! Embeds the game server in the page (default mode; a standalone server is a
//! later milestone) and drives it from the browser event loop: keyboard/mouse
//! input -> server tick -> world/agent updates -> WebGPU render.
//!
//! This crate only compiles for wasm32 (browser entry point).

#![cfg(target_arch = "wasm32")]

mod verify_gl;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlCanvasElement, HtmlDivElement, KeyboardEvent, MouseEvent, Window};

use rustcraft_client::Renderer;
use rustcraft_server::{Action, Input, Key, KeySet, Server, WorldUpdate};
use rustcraft_world::ChunkPos;

fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

struct App {
    server: Server,
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
    // Walk-mode steering state: if less than 1 block of horizontal progress
    // is made per 1s window (jitter against a slope/wall counts as stuck),
    // turn 90° — always the same way, so a full circle is covered within 4
    // episodes (the player can then find an exit from e.g. a 1-block ditch).
    walk_anchor: [f32; 2],
    /// Walk test fly phase (starts at t=30s): hold W+Space and ramp the
    /// fly speed to the max, exercising fly mode + high-speed streaming.
    walk_fly: bool,
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
    // Per-chunk cooldown for terrain-pool re-sends (avoids a ping-pong when
    // the pool stays full: (x,y,z) -> last request time, s).
    resend_cooldown: HashMap<(i32, i32, i32), f64>,
    // Keep event closures alive for the life of the page.
    _closures: Vec<*mut std::ffi::c_void>,
}

impl App {
        fn new(
        seed: u64,
        hud: HtmlDivElement,
        overlay: HtmlDivElement,
        verify_mode: bool,
        walk_mode: bool,
        npcs: Option<(u32, f32)>,
    ) -> Self {
        let mut server = Server::new(seed);
        let spawn = server.player_state().pos;
        let mut actions = Vec::new();
        if let Some((count, spacing)) = npcs {
            server.set_npc_load(count, spacing);
            // Applied on the first tick: spawn the load-test cloud.
            actions.push(Action::NpcLoad);
        }
        App {
            server,
            walk_mode,
            walk_anchor: [0.0; 2],
            walk_fly: false,
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
            resend_cooldown: HashMap::new(),
            _closures: Vec::new(),
        }
    }


    /// Run the WebGL2 shadow render and log `VERIFY_PIXELS r,g,b;...`,
    /// streaming the full frame once as base64 `VERIFY_PNG` chunks so
    /// verify.sh can reconstruct a real screenshot of the 3D scene (the
    /// WebGPU canvas itself cannot be composited in headless Chromium).
    fn run_gl_verify(&mut self) {
        let p = self.server.player_state();
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
            // the worst case for the terrain pool. From t=30s: fly phase
            // (W+Space, speed ramped to the max).
            input.keys.insert(Key::W);
            if self.walk_fly {
                input.keys.insert(Key::Space);
                self.actions.push(Action::FlyFaster); // clamps at the max
            }
            if self.pending_walk_jump {
                input.keys.insert(Key::Space);
                self.pending_walk_jump = false;
            }
            input.mouse_dx += self.pending_walk_turn;
            self.pending_walk_turn = 0.0;
        }
        self.server.set_input(input);
        for a in self.actions.drain(..) {
            self.server.push_action(a);
        }
        let t_tick = js_sys::Date::now();
        self.server.tick(dt);
        let t_mesh = js_sys::Date::now();

        let updates = self.server.take_world_updates();
        if self.verify_mode {
            for u in &updates {
                match u {
                    WorldUpdate::Chunk { pos, data } => {
                        self.verify_regions.insert(*pos, data.clone());
                    }
                }
            }
        }
        let agents = self.server.agents();
        let player = self.server.player_state();
        // The rendered camera uses exactly this state; stamp it for click
        // actions (see the `aim_*` field docs).
        self.aim_yaw = player.yaw;
        self.aim_pitch = player.pitch;
        if self.walk_mode {
            // At t=30s switch to the fly phase (max-speed straight flight).
            if self.frames_total == 1800 && !self.walk_fly {
                self.walk_fly = true;
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

        if let Some(r) = &mut self.renderer {
            r.apply_updates(updates);
            // The terrain buffer pool may have evicted chunks that are still
            // visible (rough terrain + a busy pool): ask the server to
            // re-send them (budgeted, per-chunk cooldown).
            let lost = r.take_lost();
            for pos in lost {
                let key = (pos.x, pos.y, pos.z);
                if self.resend_cooldown.get(&key).map(|t| now - *t < 2.0).unwrap_or(false) {
                    continue;
                }
                self.resend_cooldown.insert(key, now);
                if let Some(data) = self.server.resend_chunk(pos) {
                    r.requeue(WorldUpdate::Chunk { pos, data });
                }
            }
            let t_render = js_sys::Date::now();
            r.set_agents(agents);
            r.set_highlight(player.target.map(|t| [t.x, t.y, t.z]));
            let cam = [
                player.pos.x,
                player.pos.y + rustcraft_server::agent::EYE_HEIGHT,
                player.pos.z,
            ];
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
            let stats = self.server.stats();
            let fly = if player.fly {
                format!(" | FLY {:.0} b/s [F off · Q/E speed]", player.fly_speed)
            } else {
                String::new()
            };
            let (load_count, load_spacing) = self.server.npc_load_config();
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
            self.hud.set_inner_html(&format!(
                "fps {:.0} | perf tick={:.1} mesh={:.1} draw={:.1} ms/f | pos {:.0} {:.0} {:.0} | chunks {} sent / {} gen | edits {} | agents {}{}{}",
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
                fly,
                npc_line
            ));
            if self.hud_updates % 20 == 0 {
                let nf = n as u32;
                log(&format!(
                    "PERF fps={:.0} tick={:.1} mesh={:.1} render={:.1} ms/f (frames={nf})",
                    self.fps, pt, pm, pr
                ));
            }
        }
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

fn params_from_url() -> (u64, bool, bool, Option<(u32, f32)>) {
    let mut seed = 1337u64;
    let mut verify = false;
    let mut walk = false;
    // `npcs=COUNT[:SPACING]` starts the app with an NPC load already
    // spawned (headless load testing without a keyboard).
    let mut npcs: Option<(u32, f32)> = None;
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
            }
        }
    }
    (seed, verify, walk, npcs)
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

    let (seed, verify_mode, walk_mode, npcs) = params_from_url();
    log(&format!("RustCraft: app started (seed {seed})"));

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
        verify_mode,
        walk_mode,
        npcs,
    )));
    if let Some((c, s)) = npcs {
        log(&format!("RustCraft: NPC load test armed: {c} agents @ {s:.0} m spacing"));
    }

    // ---- Input events ----------------------------------------------------
    // Each closure captures its own clone of `app`; the outer Rc is never
    // moved. Closures are kept alive via raw pointers in App::_closures.
    {
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new({
            let app = app.clone();
            move |e: KeyboardEvent| {
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
        // the menu is up.
        let cb = Closure::<dyn FnMut()>::new({
            let app = app.clone();
            let canvas_for_lock = canvas.clone();
            move || {
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
