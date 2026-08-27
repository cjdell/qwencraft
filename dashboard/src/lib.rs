//! RustCraft server dashboard — a small dioxus web app (cdylib; entry
//! point is the `#[wasm_bindgen] start()` below) served by
//! `rustcraft-net` (see `dashboard/dist`, embedded into the server binary).
//!
//! Polls the same-origin status endpoints:
//!   GET /api/status  → JSON: seed, uptime, agents, event log
//!   GET /api/map     → binary: topmost block per column for a region
//! and renders a pannable (drag) / zoomable (wheel) 2D minimap with
//! players (squares) and NPCs (coloured dots) on top.


use dioxus::prelude::*;
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

// ---------------------------------------------------------------------------
// Server payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Status {
    seed: u64,
    uptime: f64,
    players: usize,
    npcs: usize,
    agents: Vec<Agent>,
    events: Vec<LogEvent>,
}

#[derive(Debug, Clone, Deserialize)]
struct Agent {
    id: u32,
    player: bool,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
    fly: bool,
    ground: bool,
    color: [u8; 3],
}

#[derive(Debug, Clone, Deserialize)]
struct LogEvent {
    t: f64,
    m: String,
}

/// One fetched map region (mirrors `rustcraft_net::map::MapRegion`).
#[derive(Clone)]
struct MapData {
    x0: i32,
    z0: i32,
    w: i32,
    h: i32,
    /// 2 bytes per column, row-major (z then x): `[y, block id]`.
    cols: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Constants (mirror the server's clamps)
// ---------------------------------------------------------------------------

const MAP_MIN: i32 = 16;
const MAP_MAX: i32 = 256;
const MIN_SCALE: f64 = 0.5; // px per block (zoomed out)
const MAX_SCALE: f64 = 8.0; // px per block (zoomed in)

/// Top-face colour per block id (mirrors `rustcraft_world::Block::color_top`).
fn block_color(b: u8) -> [u8; 3] {
    const C: [[u8; 3]; 11] = [
        [255, 255, 255], //  air (unused)
        [92, 166, 71], //   grass
        [140, 99, 66], //   dirt
        [133, 135, 140], // stone
        [222, 209, 153], // sand
        [61, 115, 217], //  water
        [148, 115, 71], //  log
        [69, 133, 51], //   leaves
        [235, 240, 247], // snowgrass
        [214, 51, 46], //   flower red
        [235, 199, 56], //  flower yellow
    ];
    C[(b as usize) % 11]
}

fn fmt_uptime(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------

/// JsValue error → human string (JsValue has no Display in wasm-bindgen 0.2).
fn js_err(e: JsValue) -> String {
    e.as_string()
        .unwrap_or_else(|| format!("{e:?}"))
}

async fn fetch_status() -> Result<Status, String> {
    let window = web_sys::window().ok_or("no window")?;
    let req = web_sys::Request::new_with_str_and_init(
        "/api/status",
        &web_sys::RequestInit::new(),
    )
    .map_err(js_err)?;
    let rv = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(js_err)?;
    let resp: web_sys::Response = rv.dyn_into::<web_sys::Response>().map_err(|_| "bad response")?;
    if resp.status() != 200 {
        return Err(format!("status: {}", resp.status()));
    }
    let v = JsFuture::from(resp.json().map_err(js_err)?)
        .await
        .map_err(js_err)?;
    serde_wasm_bindgen::from_value(v).map_err(|e| format!("status decode: {e}"))
}

async fn fetch_map(cx: i32, cz: i32, w: i32, h: i32) -> Result<MapData, String> {
    let window = web_sys::window().ok_or("no window")?;
    let url = format!("/api/map?x={cx}&z={cz}&w={w}&h={h}");
    let req = web_sys::Request::new_with_str_and_init(&url, &web_sys::RequestInit::new())
        .map_err(js_err)?;
    let rv = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(js_err)?;
    let resp: web_sys::Response = rv.dyn_into::<web_sys::Response>().map_err(|_| "bad response")?;
    if resp.status() != 200 {
        return Err(format!("map: {}", resp.status()));
    }
    // The server echoes the (clamped) region; trust it over the request.
    let origin = resp
        .headers()
        .get("x-map-origin")
        .ok()
        .flatten()
        .and_then(|s| parse_pair(&s))
        .unwrap_or((cx - w / 2, cz - h / 2));
    let (rw, rh) = resp
        .headers()
        .get("x-map-size")
        .ok()
        .flatten()
        .and_then(|s| parse_pair(&s))
        .unwrap_or((w, h));
    let bv = JsFuture::from(resp.array_buffer().map_err(js_err)?)
        .await
        .map_err(js_err)?;
    let buf = bv
        .dyn_into::<js_sys::ArrayBuffer>()
        .map_err(|_| "bad array buffer")?;
    let cols = js_sys::Uint8Array::new(&buf).to_vec();
    let need = 2 * rw as usize * rh as usize;
    if cols.len() < need {
        return Err(format!("short map body: {} < {need}", cols.len()));
    }
    Ok(MapData {
        x0: origin.0,
        z0: origin.1,
        w: rw,
        h: rh,
        cols: cols[..need].to_vec(),
    })
}

fn parse_pair(s: &str) -> Option<(i32, i32)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Await a JS `setTimeout` (dioxus-web ships no sleep helper).
async fn sleep_ms(ms: f64) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let cb = wasm_bindgen::closure::Closure::once(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        let f: &js_sys::Function = cb.as_ref().dyn_ref::<js_sys::Function>().unwrap();
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(f, ms as i32);
        cb.forget();
    });
    let _ = JsFuture::from(promise).await;
}

// ---------------------------------------------------------------------------
// View / map-request math
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct View {
    cx: f64,
    cz: f64,
    /// Px per block (zoom).
    scale: f64,
}

/// The map region the dashboard should fetch for a view + canvas size
/// (clamped the same way the server does).
fn map_request(cx: f64, cz: f64, scale: f64, cw: u32, ch: u32) -> (i32, i32, i32, i32) {
    let w = ((cw as f64 / scale).ceil() as i32).clamp(MAP_MIN, MAP_MAX);
    let h = ((ch as f64 / scale).ceil() as i32).clamp(MAP_MIN, MAP_MAX);
    (cx.round() as i32, cz.round() as i32, w, h)
}

/// Wheel zoom anchored at the cursor: the world point under the mouse stays
/// under the mouse.
fn zoom_around(v: View, px: f64, py: f64, cw: f64, ch: f64, delta: f64) -> View {
    let factor = if delta < 0.0 { 1.25 } else { 1.0 / 1.25 };
    let scale = (v.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
    if (scale - v.scale).abs() < 1e-9 {
        return v;
    }
    let (cx_i, cz_i, w, h) = map_request(v.cx, v.cz, v.scale, cw as u32, ch as u32);
    let x0 = (cx_i - w / 2) as f64;
    let z0 = (cz_i - h / 2) as f64;
    // World point under the cursor, before the zoom.
    let wx = x0 + (px / cw) * w as f64;
    let wz = z0 + (py / ch) * h as f64;
    let (_, _, nw, nh) = map_request(0.0, 0.0, scale, cw as u32, ch as u32);
    View {
        cx: (wx + nw as f64 / 2.0 - (px / cw) * nw as f64).round(),
        cz: (wz + nh as f64 / 2.0 - (py / ch) * nh as f64).round(),
        scale,
    }
}

// ---------------------------------------------------------------------------
// Canvas painting
// ---------------------------------------------------------------------------

/// Paint the map region into the (hidden) offscreen canvas as raw pixels —
/// done once per map fetch, never per frame.
fn paint_offscreen(off: &web_sys::HtmlCanvasElement, m: &MapData) {
    off.set_width(m.w.max(1) as u32);
    off.set_height(m.h.max(1) as u32);
    let Some(ctx) = off
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
    else {
        return;
    };
    let n = (m.w * m.h) as usize;
    let mut px = vec![0u8; n * 4];
    for i in 0..n {
        let y = m.cols[2 * i] as i32;
        let b = m.cols[2 * i + 1];
        let [r, g, bl] = block_color(b);
        // Subtle relief: higher = brighter.
        let shade = (0.62 + 0.012 * y.clamp(0, 48) as f32).min(1.15);
        let o = i * 4;
        px[o] = (r as f32 * shade) as u8;
        px[o + 1] = (g as f32 * shade) as u8;
        px[o + 2] = (bl as f32 * shade) as u8;
        px[o + 3] = 255;
    }
    let clamped = js_sys::Uint8ClampedArray::from(&px[..]);
    if let Some(img) =
        web_sys::ImageData::new_with_js_u8_clamped_array(&clamped, m.w.max(1) as u32).ok()
    {
        let _ = ctx.put_image_data(&img, 0.0, 0.0);
    }
}

/// Draw one frame: the (stretched) map image, the 16-block chunk grid, and
/// the agents on top.
fn draw(
    ctx: &web_sys::CanvasRenderingContext2d,
    canvas: &web_sys::HtmlCanvasElement,
    m: Option<&MapData>,
    off: Option<&web_sys::HtmlCanvasElement>,
    agents: &[Agent],
) {
    let w = canvas.width() as f64;
    let h = canvas.height() as f64;
    ctx.set_fill_style_str("#0d1117");
    ctx.fill_rect(0.0, 0.0, w, h);
    if w < 2.0 || h < 2.0 {
        return;
    }

    let (x0, z0, mw, mh) = match (m, off) {
        (Some(m), Some(off)) => {
            ctx.set_image_smoothing_enabled(false);
            let _ = ctx
                .draw_image_with_html_canvas_element_and_dw_and_dh(off, 0.0, 0.0, w, h);
            (m.x0 as f64, m.z0 as f64, m.w as f64, m.h as f64)
        }
        _ => return,
    };
    let sx = |wx: f64| (wx - x0) / mw * w;
    let sy = |wz: f64| (wz - z0) / mh * h;

    // Chunk grid (16-block lines).
    ctx.set_stroke_style_str("rgba(255,255,255,0.08)");
    ctx.set_line_width(1.0);
    let x1 = x0 + mw;
    let z1 = z0 + mh;
    let gx0 = ((x0 / 16.0).floor() * 16.0) as i32;
    let gz0 = ((z0 / 16.0).floor() * 16.0) as i32;
    for gx in (gx0..x1 as i32 + 16).step_by(16) {
        if gx < x0 as i32 {
            continue;
        }
        let px = sx(gx as f64);
        ctx.begin_path();
        ctx.move_to(px, 0.0);
        ctx.line_to(px, h);
        ctx.stroke();
    }
    for gz in (gz0..z1 as i32 + 16).step_by(16) {
        if gz < z0 as i32 {
            continue;
        }
        let py = sy(gz as f64);
        ctx.begin_path();
        ctx.move_to(0.0, py);
        ctx.line_to(w, py);
        ctx.stroke();
    }

    // NPCs first (small), players on top (bigger, labelled).
    for a in agents.iter().filter(|a| !a.player) {
        let (px, py) = (sx(a.x as f64), sy(a.z as f64));
        if px < -8.0 || py < -8.0 || px > w + 8.0 || py > h + 8.0 {
            continue;
        }
        let c = format!("#{:02x}{:02x}{:02x}", a.color[0], a.color[1], a.color[2]);
        ctx.set_fill_style_str(&c);
        ctx.fill_rect(px - 2.0, py - 2.0, 4.0, 4.0);
    }
    for a in agents.iter().filter(|a| a.player) {
        let (px, py) = (sx(a.x as f64), sy(a.z as f64));
        if px < -40.0 || py < -40.0 || px > w + 40.0 || py > h + 40.0 {
            continue;
        }
        ctx.set_fill_style_str("#eaf6ff");
        ctx.fill_rect(px - 4.0, py - 4.0, 8.0, 8.0);
        ctx.set_stroke_style_str("#2aa198");
        ctx.set_line_width(2.0);
        ctx.stroke_rect(px - 4.0, py - 4.0, 8.0, 8.0);
        // Facing tick (yaw=0 faces -Z, i.e. up on the map).
        let dx = a.yaw.sin() as f64;
        let dz = -(a.yaw.cos() as f64);
        ctx.set_line_width(1.5);
        ctx.begin_path();
        ctx.move_to(px, py);
        ctx.line_to(px + dx * 11.0, py + dz * 11.0);
        ctx.stroke();
        ctx.set_fill_style_str("#ffffff");
        ctx.set_font("11px ui-monospace, monospace");
        let _ = ctx.fill_text(&format!("P{}", a.id), px + 7.0, py - 7.0);
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[component]
fn App() -> Element {
    let mut status: Signal<Option<Status>> = use_signal(|| None);
    let mut map: Signal<Option<MapData>> = use_signal(|| None);
    let mut view: Signal<View> = use_signal(|| View { cx: 8.0, cz: 8.0, scale: 3.0 });
    let mut online: Signal<bool> = use_signal(|| false);
    let mut csize: Signal<(u32, u32)> = use_signal(|| (0, 0));
    let mut drag: Signal<Option<(f64, f64)>> = use_signal(|| None);
    // dioxus 0.7 has no node-ref hook: the canvas/log elements carry ids
    // (below) and are looked up once they exist.
    let mut canvas_el: Signal<Option<web_sys::HtmlCanvasElement>> = use_signal(|| None);
    let mut log_el: Signal<Option<web_sys::Element>> = use_signal(|| None);

    use_effect(move || {
        let doc = web_sys::window().and_then(|w| w.document());
        if canvas_el.read().is_none() {
            if let Some(el) = doc.as_ref().and_then(|d| d.get_element_by_id("map-canvas")) {
                *canvas_el.write() =
                    Some(el.unchecked_into::<web_sys::HtmlCanvasElement>());
            }
        }
        if log_el.read().is_none() {
            if let Some(el) = doc.as_ref().and_then(|d| d.get_element_by_id("event-log")) {
                *log_el.write() = Some(el);
            }
        }
    });
    // Hidden canvas that holds the current map region as pixels.
    let off: Signal<Option<web_sys::HtmlCanvasElement>> = use_signal(|| {
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.create_element("canvas").ok())
            .map(|c| c.unchecked_into::<web_sys::HtmlCanvasElement>())
    });

    // Status poll (1 s).
    use_effect(move || {
        spawn(async move {
            loop {
                match fetch_status().await {
                    Ok(s) => {
                        *status.write() = Some(s);
                        *online.write() = true;
                    }
                    Err(_) => *online.write() = false,
                }
                sleep_ms(1000.0).await;
            }
        });
    });

    // Map poll (250 ms tick): refetch when the view/canvas changes, plus a
    // slow 3 s refresh so block edits appear without interaction.
    use_effect(move || {
        spawn(async move {
            let mut last_req: Option<(i32, i32, i32, i32)> = None;
            // js_sys::Date::now (ms): std::time::Instant panics on wasm32.
            // Pre-expired so the first tick fetches immediately.
            let mut last_fetch = js_sys::Date::now() - 10_000.0;
            loop {
                // Keep the canvas sized to its pane (setting the size clears
                // it; the draw effect repaints). clientWidth is the CSS size.
                if let Some(canvas) = (*canvas_el.read()).clone() {
                    let (nw, nh) = (canvas.client_width().max(1) as u32, canvas.client_height().max(1) as u32);
                    if nw != canvas.width() || nh != canvas.height() {
                        canvas.set_width(nw);
                        canvas.set_height(nh);
                    }
                    if *csize.read() != (nw, nh) {
                        *csize.write() = (nw, nh);
                    }
                }
                let v = *view.read();
                let (cw, ch) = *csize.read();
                if cw > 1 && ch > 1 {
                    let req = map_request(v.cx, v.cz, v.scale, cw, ch);
                    let now = js_sys::Date::now();
                    let due = last_req != Some(req) || now - last_fetch > 3_000.0;
                    if due {
                        last_fetch = now;
                        if let Ok(m) = fetch_map(req.0, req.1, req.2, req.3).await {
                            last_req = Some(req);
                            if let Some(o) = (*off.read()).clone() {
                                paint_offscreen(&o, &m);
                            }
                            *map.write() = Some(m);
                        }
                    }
                }
                sleep_ms(250.0).await;
            }
        });
    });

    // Auto-scroll the event log to the newest line (scrollTop clamps to
    // the maximum, so a huge value means "to the bottom").
    use_effect(move || {
        let _count = status.read().as_ref().map(|s| s.events.len()).unwrap_or(0);
        if let Some(el) = (*log_el.read()).clone() {
            el.set_scroll_top(i32::MAX);
        }
    });

    // Redraw on any input change (map, agents, view, canvas size).
    use_effect(move || {
        let v = *view.read();
        let csize = *csize.read();
        let m = (*map.read()).clone();
        let agents: Vec<Agent> = status
            .read()
            .as_ref()
            .map(|s| s.agents.clone())
            .unwrap_or_default();
        let _ = (v, csize); // dependencies registered by the reads above
        let Some(canvas) = (*canvas_el.read()).clone() else {
            return;
        };
        let Some(ctx) = canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
        else {
            return;
        };
        let offc = (*off.read()).clone();
        draw(&ctx, &canvas, m.as_ref(), offc.as_ref(), &agents);
    });

    let st = (*status.read()).clone();
    let online = *online.read();
    let view_now = *view.read();
    let events: Vec<LogEvent> = st.as_ref().map(|s| s.events.clone()).unwrap_or_default();
    let players: Vec<Agent> = st
        .as_ref()
        .map(|s| s.agents.iter().filter(|a| a.player).cloned().collect())
        .unwrap_or_default();
    let player_text = match st.as_ref().map(|s| s.players) {
        Some(1) => "1 player connected".to_string(),
        Some(n) => format!("{n} players connected"),
        None => String::new(),
    };
    let hud = format!(
        "{}% · center ({}, {})",
        (view_now.scale * 100.0).round() as i32,
        view_now.cx.round() as i32,
        view_now.cz.round() as i32,
    );

    rsx! {
        div { class: "app",
            header { class: "topbar",
                div { class: "title", "RustCraft server" }
                div { class: "stat",
                    span { class: if online { "dot on" } else { "dot off" } }
                    { if online { "connected" } else { "connecting…" } }
                }
                if let Some(st) = &st {
                    div { class: "stat", "{st.seed} seed" }
                    div { class: "stat players", "{player_text}" }
                    div { class: "stat", "{st.npcs} npcs" }
                    div { class: "stat", "up {fmt_uptime(st.uptime)}" }
                }
            }
            div { class: "body",
                div { class: "side",
                    div { class: "side-head", "EVENTS" }
                    div { class: "events", id: "event-log",
                        if events.is_empty() {
                            div { class: "empty", "no events yet" }
                        }
                        for e in events {
                            div { class: "event",
                                span { class: "t", "{fmt_uptime(e.t)}" }
                                span { class: "m", "{e.m}" }
                            }
                        }
                    }
                    div { class: "side-head", "PLAYERS" }
                    div { class: "players",
                        if players.is_empty() {
                            div { class: "empty", "no players connected" }
                        }
                        for a in players {
                            div { class: "player-row",
                                span { class: "pid", "P{a.id}" }
                                span { class: "pos",
                                    "{a.x.round() as i32}, {a.z.round() as i32} · y{a.y.round() as i32}"
                                }
                                if a.fly { span { class: "fly", "fly" } }
                                if !a.ground && !a.fly { span { class: "air", "air" } }
                                button {
                                    class: "focus",
                                    title: "center map on player",
                                    onclick: move |_| {
                                        if let Some(st) = (*status.read()).clone() {
                                            if let Some(pa) =
                                                st.agents.iter().find(|x| x.id == a.id)
                                            {
                                                *view.write() = View {
                                                    cx: pa.x as f64,
                                                    cz: pa.z as f64,
                                                    scale: MAX_SCALE.min(4.0),
                                                };
                                            }
                                        }
                                    },
                                    "◎"
                                }
                            }
                        }
                    }
                }
                div { class: "map-pane",
                    canvas {
                        id: "map-canvas",
                        class: "map",
                        onmousedown: move |e: MouseEvent| {
                            e.prevent_default();
                            let c = e.client_coordinates();
                            *drag.write() = Some((c.x, c.y));
                        },
                        onmousemove: move |e: MouseEvent| {
                            let Some((lx, ly)) = *drag.read() else {
                                return;
                            };
                            let c = e.client_coordinates();
                            *drag.write() = Some((c.x, c.y));
                            let (dx, dy) = (c.x - lx, c.y - ly);
                            let (cw, ch) = *csize.read();
                            // Blocks per pixel per axis (the map is stretched
                            // to the canvas; the axes match except when the
                            // region is clamped at the max side).
                            let (bx, bz) = match (*map.read()).clone() {
                                Some(m) => (m.w as f64 / cw as f64, m.h as f64 / ch as f64),
                                None => {
                                    let s = view.read().scale;
                                    (1.0 / s, 1.0 / s)
                                }
                            };
                            let v = *view.read();
                            *view.write() = View {
                                cx: v.cx - dx * bx,
                                cz: v.cz - dy * bz,
                                scale: v.scale,
                            };
                        },
                        onmouseup: move |_| { *drag.write() = None; },
                        onmouseleave: move |_| { *drag.write() = None; },
                        onwheel: move |e: WheelEvent| {
                            e.prevent_default();
                            let Some(canvas) = (*canvas_el.read()).clone() else {
                                return;
                            };
                            let delta_y = match e.delta() {
                                dioxus::html::geometry::WheelDelta::Pixels(v) => v.y,
                                _ => 0.0,
                            };
                            let p = e.element_coordinates();
                            let v = *view.read();
                            *view.write() = zoom_around(
                                v,
                                p.x.max(0.0),
                                p.y.max(0.0),
                                canvas.width() as f64,
                                canvas.height() as f64,
                                delta_y,
                            );
                        },
                    }
                    div { class: "map-hud", { hud }
                        span { class: "hint", "drag to pan · wheel to zoom" }
                    }
                }
            }
        }
    }
}

/// The JS entry point (called from index.html after `init()`):
/// `import init, { start } from "./rustcraft-dashboard.js"; init().then(start);`
///
/// A cdylib with an explicit `#[wasm_bindgen]` entry is used instead of a
/// binary's `main`: the wasm-bindgen glue only re-exports `#[wasm_bindgen]`
/// functions, and a binary's `main` (which takes argv) is not one.
#[wasm_bindgen]
pub fn start() {
    console_error_panic_hook::set_once();
    launch(App);
}
