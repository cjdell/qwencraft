//! Qwencraft server dashboard — a small dioxus web app (cdylib; entry
//! point is the `#[wasm_bindgen] start()` below) served by
//! `qwencraft-net` (see `dashboard/dist`, embedded into the server binary).
//!
//! Polls the same-origin status endpoints:
//!   GET /api/status  → JSON: seed, uptime, agents, event log
//!   GET /api/map     → binary: topmost block per column for a region
//! and renders a 2D minimap with players (squares) and NPCs (coloured
//! dots) on top. The map is hillshaded (light from the upper-left) with
//! contour lines (minor every 4 blocks, major every 16), and pan/zoom are
//! continuous at any fractional scale (0.5–8 px per block): drag or
//! two-finger scroll pans, trackpad pinch / mouse wheel zooms at the
//! cursor.
//!
//! The server serves map regions of at most 256×256 blocks, so the
//! dashboard fetches the visible area as a mosaic of 256-aligned **tiles**
//! (cached, centre-first, a few per tick) and draws them all — deep
//! zoom-out fills the whole canvas instead of a small central square.
//! Beyond `MAX_SPAN` blocks of world per side the canvas letterboxes.


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
    /// Display name (players only; empty for NPCs). `default` keeps older
    /// server payloads (without the field) decodable.
    #[serde(default)]
    name: String,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
    fly: bool,
    ground: bool,
    color: [u8; 3],
}

impl Agent {
    /// Name for display (players have one; fall back to the id tag).
    fn label(&self) -> String {
        if self.player && !self.name.is_empty() {
            self.name.clone()
        } else {
            format!("P{}", self.id)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LogEvent {
    t: f64,
    m: String,
}

/// One fetched map region (mirrors `qwencraft_net::map::MapRegion`).
#[derive(Clone)]
struct MapData {
    x0: i32,
    z0: i32,
    w: i32,
    h: i32,
    /// 2 bytes per column, row-major (z then x): `[y, block id]`.
    cols: Vec<u8>,
}

/// One cached map tile: the region's bounds + its painted pixels (a hidden
/// offscreen canvas, 1 px per block) + fetch time (for the refresh cycle).
/// The raw column bytes live only in the fetch task (they're consumed by
/// `paint_offscreen`); this struct is cheap to clone (small ints + one JS
/// handle), which keeps it signal-friendly.
#[derive(Clone)]
struct Tile {
    x0: i32,
    z0: i32,
    w: i32,
    h: i32,
    off: web_sys::HtmlCanvasElement,
    /// `js_sys::Date::now()` ms when fetched.
    at: f64,
}

// ---------------------------------------------------------------------------
// Constants (mirror the server's clamps)
// ---------------------------------------------------------------------------

/// The server's max region side (256) — also the dashboard's tile size
/// (a tile request is exactly one 256×256 region, so it never clamps).
const TILE: i32 = 256;
/// Max world span (blocks per side) the dashboard fetches at once. At the
/// minimum zoom (0.5 px/block) a 1280-px-wide pane would want 2560 blocks;
/// beyond this cap the canvas letterboxes (the scale stays honest).
const MAX_SPAN: i32 = 2048;
const MIN_SCALE: f64 = 0.5; // px per block (zoomed out)
const MAX_SCALE: f64 = 8.0; // px per block (zoomed in)
/// `Block::Water` id (mirrors `qwencraft_world::Block`).
const WATER: u8 = 5;
/// Contour spacing in blocks: minor lines, and major lines.
const CONTOUR_MINOR: i32 = 4;
const CONTOUR_MAJOR: i32 = 16;
/// Blocks of slack fetched around the visible rect (a fast pan outruns
/// the refetch by a few blocks before the new tiles land).
const MAP_MARGIN: f64 = 4.0;
/// Tiles (of `TILE` blocks) re-fetched this often so block edits made by
/// players appear on the map without interaction.
const TILE_REFRESH_MS: f64 = 3_000.0;
/// Cap on the number of cached tiles (memory + refresh traffic bound).
const MAX_TILES: usize = 100;
/// Tiles fetched per poll tick (a fresh deep-zoom view fills in smoothly
/// over ~1 s instead of bursting 40+ requests at once).
const TILES_PER_TICK: usize = 6;

/// Top-face colour per block id (mirrors `qwencraft_world::Block::color_top`).
fn block_color(b: u8) -> [u8; 3] {
    const C: [[u8; 3]; 17] = [
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
        [184, 140, 87], //  planks
        [140, 143, 148], // cobblestone
        [158, 77, 61], //   brick
        [191, 217, 230], // glass
        [217, 89, 64], //   tnt
        [41, 26, 51], //    obsidian
    ];
    C[(b as usize) % 17]
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

/// Floor division for possibly-negative block coordinates (Rust's `/`
/// truncates toward zero, which would misalign tiles on the negative side).
fn floor_div(a: i32, b: i32) -> i32 {
    let q = a / b;
    if a % b < 0 {
        q - 1
    } else {
        q
    }
}

/// The `TILE`-aligned tiles covering the visible rect (plus `MAP_MARGIN`
/// blocks of slack on every side), span-capped to `MAX_SPAN` blocks per
/// side and ordered nearest-to-centre first (the middle of the view fills
/// in before the edges do).
///
/// Each tile is one 256×256 fetch (the server's max region), so the deep
/// zoom-out that used to letterbox a small square in the middle now draws
/// the whole canvas as a mosaic.
fn tiles_for_view(cx: f64, cz: f64, scale: f64, cw: u32, ch: u32) -> Vec<(i32, i32)> {
    let vw = (cw as f64) / scale;
    let vh = (ch as f64) / scale;
    let mut x0 = (cx - vw / 2.0 - MAP_MARGIN).floor();
    let mut x1 = (cx + vw / 2.0 + MAP_MARGIN).ceil();
    let mut z0 = (cz - vh / 2.0 - MAP_MARGIN).floor();
    let mut z1 = (cz + vh / 2.0 + MAP_MARGIN).ceil();
    // Span cap: centre the fetchable span on the view centre.
    if x1 - x0 > MAX_SPAN as f64 {
        let c = (x0 + x1) / 2.0;
        x0 = c - (MAX_SPAN as f64) / 2.0;
        x1 = c + (MAX_SPAN as f64) / 2.0;
    }
    if z1 - z0 > MAX_SPAN as f64 {
        let c = (z0 + z1) / 2.0;
        z0 = c - (MAX_SPAN as f64) / 2.0;
        z1 = c + (MAX_SPAN as f64) / 2.0;
    }
    let (ix0, ix1) = (floor_div(x0 as i32, TILE), floor_div(x1 as i32 - 1, TILE));
    let (iz0, iz1) = (floor_div(z0 as i32, TILE), floor_div(z1 as i32 - 1, TILE));
    let mut list = Vec::with_capacity((ix1 - ix0 + 1) as usize * (iz1 - iz0 + 1) as usize);
    for tz in iz0..=iz1 {
        for tx in ix0..=ix1 {
            list.push((tx, tz));
        }
    }
    // Nearest to the view centre first (the tile centres are tile*256+128).
    list.sort_by(|a, b| {
        let da = ((a.0 as f64 * (TILE as f64) + (TILE as f64) / 2.0) - cx)
            .abs()
            .max(((a.1 as f64 * (TILE as f64) + (TILE as f64) / 2.0) - cz).abs());
        let db = ((b.0 as f64 * (TILE as f64) + (TILE as f64) / 2.0) - cx)
            .abs()
            .max(((b.1 as f64 * (TILE as f64) + (TILE as f64) / 2.0) - cz).abs());
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    list
}

/// The initial zoom for a pane of `pane_w` CSS px: a view that fills it
/// (~240 blocks wide), unless `?zoom=N` (percent) says otherwise — clamped
/// to the supported range.
fn initial_scale(pane_w: f64) -> f64 {
    let default = (pane_w / 240.0).clamp(MIN_SCALE, MAX_SCALE);
    let Some(window) = web_sys::window() else {
        return default;
    };
    let Ok(search) = window.location().search() else {
        return default;
    };
    let Some(query) = search.strip_prefix('?') else {
        return default;
    };
    for kv in query.split('&') {
        if let Some(v) = kv.strip_prefix("zoom=") {
            if let Ok(pct) = v.parse::<f64>() {
                return (pct / 100.0).clamp(MIN_SCALE, MAX_SCALE);
            }
        }
    }
    default
}

/// Zoom anchored at the cursor: the world point under the mouse stays under
/// the mouse, at any fractional scale in `MIN_SCALE..=MAX_SCALE`.
fn zoom_around(v: View, px: f64, py: f64, cw: f64, ch: f64, factor: f64) -> View {
    let scale = (v.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
    if (scale - v.scale).abs() < 1e-12 {
        return v;
    }
    // World point under the cursor, before the zoom.
    let wx = v.cx - cw / (2.0 * v.scale) + px / v.scale;
    let wz = v.cz - ch / (2.0 * v.scale) + py / v.scale;
    // New centre that keeps that point under the cursor.
    View {
        cx: wx + cw / (2.0 * scale) - px / scale,
        cz: wz + ch / (2.0 * scale) - py / scale,
        scale,
    }
}

// ---------------------------------------------------------------------------
// Canvas painting
// ---------------------------------------------------------------------------

/// Paint the map region into the (hidden) offscreen canvas as raw pixels —
/// 1 px per block, done once per map fetch, never per frame.
///
/// Elevation is depicted two ways:
/// - hillshading: each pixel's surface normal (central-difference
///   gradient of the height field) is lit from the upper-left, so slopes
///   facing away from the light read darker — relief is visible even at
///   low zoom;
/// - contour lines: a pixel at a surface height that is a multiple of
///   `CONTOUR_MINOR` (resp. `CONTOUR_MAJOR`) blocks is darkened. The lines
///   are 1 px wide in block space, so they fade out naturally below 100%
///   zoom and read as a proper topo map zoomed in. Water is skipped (its
///   surface is flat at the sea level, so a contour would darken whole
///   lakes).
fn paint_offscreen(off: &web_sys::HtmlCanvasElement, m: &MapData) {
    let w = m.w.max(1) as usize;
    let h = m.h.max(1) as usize;
    off.set_width(w as u32);
    off.set_height(h as u32);
    let Some(ctx) = off
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
    else {
        return;
    };
    // Height of a column, edge-clamped (the ±1 halo for the gradient uses
    // repeated edge values; the visible rect starts MAP_MARGIN blocks in).
    let hgt = |x: isize, z: isize| -> i32 {
        let x = x.clamp(0, (w - 1) as isize);
        let z = z.clamp(0, (h - 1) as isize);
        m.cols[2 * (((z * w as isize) + x) as usize)] as i32
    };
    // Light from the upper-left (north-west), steep-ish.
    let (lx, ly, lz) = {
        let (a, b, c) = (-0.5f32, 0.8f32, -0.35f32);
        let l = (a * a + b * b + c * c).sqrt();
        (a / l, b / l, c / l)
    };
    let n = w * h;
    let mut px = vec![0u8; n * 4];
    for z in 0..h {
        for x in 0..w {
            let i = z * w + x;
            let y = hgt(x as isize, z as isize);
            let b = m.cols[2 * i + 1];
            let [r, g, bl] = block_color(b);
            // Hillshade: surface normal from the central-difference
            // gradient (dh per block on each axis).
            let gx = (hgt(x as isize + 1, z as isize) - hgt(x as isize - 1, z as isize)) as f32
                * 0.5;
            let gz =
                (hgt(x as isize, z as isize + 1) - hgt(x as isize, z as isize - 1)) as f32 * 0.5;
            let (nx, ny, nz) = {
                let (a, b, c) = (-gx, 1.0f32, -gz);
                let l = (a * a + b * b + c * c).sqrt();
                (a / l, b / l, c / l)
            };
            let hill = (nx * lx + ny * ly + nz * lz).clamp(0.0, 1.0);
            // Hillshade + a slight height ramp (high ground reads brighter).
            let mut f = (0.55 + 0.45 * hill) * (0.92 + 0.004 * y.clamp(0, 64) as f32);
            // Contour lines (see above).
            if b != WATER && y != 255 {
                if y % CONTOUR_MAJOR == 0 {
                    f *= 0.60;
                } else if y % CONTOUR_MINOR == 0 {
                    f *= 0.84;
                }
            }
            let o = i * 4;
            px[o] = (r as f32 * f).min(255.0) as u8;
            px[o + 1] = (g as f32 * f).min(255.0) as u8;
            px[o + 2] = (bl as f32 * f).min(255.0) as u8;
            px[o + 3] = 255;
        }
    }
    let clamped = js_sys::Uint8ClampedArray::from(&px[..]);
    if let Some(img) = web_sys::ImageData::new_with_js_u8_clamped_array(&clamped, w as u32).ok() {
        let _ = ctx.put_image_data(&img, 0.0, 0.0);
    }
}

/// Draw one frame: the map tiles (each bilinear-scaled from its
/// 1px-per-block offscreen buffer at a fractional offset, so pan and zoom
/// are smooth at any scale), the 16-block chunk grid, and the agents on
/// top. Tiles arrive centre-first over the first poll ticks, so a fresh
/// deep-zoom view fills in smoothly; the grid and agents render even
/// before the first tile lands.
fn draw(
    ctx: &web_sys::CanvasRenderingContext2d,
    canvas: &web_sys::HtmlCanvasElement,
    v: &View,
    tiles: &std::collections::HashMap<(i32, i32), Tile>,
    agents: &[Agent],
) {
    let w = canvas.width() as f64;
    let h = canvas.height() as f64;
    ctx.set_fill_style_str("#0d1117");
    ctx.fill_rect(0.0, 0.0, w, h);
    if w < 2.0 || h < 2.0 {
        return;
    }

    // Visible world rect at the current scale: 1 block = v.scale px, with
    // fractional offsets (sub-block pan positions stay smooth).
    let s = v.scale;
    let wx0 = v.cx - w / (2.0 * s);
    let wz0 = v.cz - h / (2.0 * s);
    let sx = |wx: f64| (wx - wx0) * s;
    let sy = |wz: f64| (wz - wz0) * s;

    // The map tiles (1px per block each). Bilinear scaling makes the
    // fractional zoom smooth ("texture zoom"); tiles beyond the span cap
    // simply aren't fetched, so the canvas letterboxes there — the scale
    // shown in the HUD is still the real one.
    ctx.set_image_smoothing_enabled(true);
    for t in tiles.values() {
        let dx = sx(t.x0 as f64);
        let dy = sy(t.z0 as f64);
        let dw = t.w as f64 * s;
        let dh = t.h as f64 * s;
        // Cull tiles fully outside the canvas (deep zoom-out fetches the
        // whole span cap; only the ones intersecting the pane are drawn).
        if dx + dw < 0.0 || dy + dh < 0.0 || dx > w || dy > h {
            continue;
        }
        let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
            &t.off,
            0.0,
            0.0,
            t.w.max(1) as f64,
            t.h.max(1) as f64,
            dx,
            dy,
            dw,
            dh,
        );
    }

    // Chunk grid (16-block lines) across the visible rect.
    ctx.set_stroke_style_str("rgba(255,255,255,0.08)");
    ctx.set_line_width(1.0);
    let x1 = wx0 + w / s;
    let z1 = wz0 + h / s;
    let mut gx = (wx0 / 16.0).floor() * 16.0;
    while gx <= x1 {
        let px = sx(gx);
        ctx.begin_path();
        ctx.move_to(px, 0.0);
        ctx.line_to(px, h);
        ctx.stroke();
        gx += 16.0;
    }
    let mut gz = (wz0 / 16.0).floor() * 16.0;
    while gz <= z1 {
        let py = sy(gz);
        ctx.begin_path();
        ctx.move_to(0.0, py);
        ctx.line_to(w, py);
        ctx.stroke();
        gz += 16.0;
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
        let _ = ctx.fill_text(&a.label(), px + 7.0, py - 7.0);
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[component]
fn App() -> Element {
    let mut status: Signal<Option<Status>> = use_signal(|| None);
    // Cached map tiles, keyed by tile origin (tile index × 256 blocks).
    // The fetch task (below) is the only writer; the draw effect reads.
    let mut tiles: Signal<std::collections::HashMap<(i32, i32), Tile>> =
        use_signal(std::collections::HashMap::new);
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
    // Swallow browser-level wheel side effects everywhere in the app:
    // horizontal-dominant swipes (the trackpad back/forward gesture) and
    // ctrl+wheel (page zoom). The map canvas handles its own wheel events;
    // without this, an accidental left swipe over the top bar or the event
    // log navigates the browser. Registered on window with `passive: false`
    // — Chrome defaults window-level wheel listeners to passive, where
    // preventDefault would be ignored.
    use_effect(move || {
        let Some(window) = web_sys::window() else {
            return;
        };
        let closure = wasm_bindgen::closure::Closure::wrap(
            Box::new(move |e: web_sys::WheelEvent| {
                if e.ctrl_key() || e.delta_x().abs() > e.delta_y().abs() {
                    e.prevent_default();
                }
            }) as Box<dyn FnMut(web_sys::WheelEvent)>,
        );
        let opts = web_sys::AddEventListenerOptions::new();
        opts.set_passive(false);
        let _ = window
            .add_event_listener_with_callback_and_add_event_listener_options(
                "wheel",
                closure.as_ref().unchecked_ref::<js_sys::Function>(),
                &opts,
            );
        closure.forget();
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

    // Map poll (100 ms tick): keeps the canvas sized, then fetches the
    // tiles the view needs (centre-first, a few per tick) and refreshes
    // in-view tiles every `TILE_REFRESH_MS` so block edits appear without
    // interaction. Sub-block pans do NOT change the tile set (the drawn
    // offset moves continuously), so panning only fetches when a new tile
    // edge is crossed.
    use_effect(move || {
        spawn(async move {
            let mut fitted = false;
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
                    // First time we know the pane size: default to a view
                    // that fills it (~240 blocks wide) instead of an
                    // arbitrary fixed scale. ?zoom=N (percent: 50 = 0.5
                    // px/block, 800 = 8) overrides it — handy for
                    // deep-linking a view and for the zoom-out test.
                    if !fitted && nw > 1 && nh > 1 {
                        fitted = true;
                        let scale = initial_scale(nw as f64);
                        *view.write() = View { cx: 8.0, cz: 8.0, scale };
                    }
                }
                let v = *view.read();
                let (cw, ch) = *csize.read();
                if cw > 1 && ch > 1 {
                    // Tiles the view needs, centre-first: fetch the missing
                    // ones (and the stale ones) up to a per-tick budget, so
                    // a fresh deep-zoom view fills in smoothly over ~1 s
                    // instead of bursting 40+ requests at once.
                    let needed = tiles_for_view(v.cx, v.cz, v.scale, cw, ch);
                    let now = js_sys::Date::now();
                    let mut budget = TILES_PER_TICK;
                    for &key in &needed {
                        if budget == 0 {
                            break;
                        }
                        let due = match tiles.read().get(&key) {
                            None => true,
                            Some(t) => now - t.at >= TILE_REFRESH_MS,
                        };
                        if !due {
                            continue;
                        }
                        budget -= 1;
                        let (tx, tz) = key;
                        // A tile is exactly one server-max region, so the
                        // request never clamps.
                        if let Ok(m) = fetch_map(tx * TILE + TILE / 2, tz * TILE + TILE / 2, TILE, TILE).await {
                            let off = match tiles.read().get(&key) {
                                Some(t) => t.off.clone(),
                                None => match web_sys::window()
                                    .and_then(|w| w.document())
                                    .and_then(|d| d.create_element("canvas").ok())
                                {
                                    Some(c) => c.unchecked_into::<web_sys::HtmlCanvasElement>(),
                                    None => continue,
                                },
                            };
                            paint_offscreen(&off, &m);
                            tiles.write().insert(
                                key,
                                Tile {
                                    x0: m.x0,
                                    z0: m.z0,
                                    w: m.w,
                                    h: m.h,
                                    off,
                                    at: js_sys::Date::now(),
                                },
                            );
                        }
                    }
                    // Bound the cache: when over the cap, keep the tiles
                    // nearest the view centre (deep zoom-out is the only
                    // way to get there).
                    if tiles.read().len() > MAX_TILES {
                        let mut by_dist: Vec<_> = tiles
                            .read()
                            .keys()
                            .map(|&(tx, tz)| {
                                let d = ((tx * TILE + TILE / 2) as f64 - v.cx)
                                    .abs()
                                    .max(((tz * TILE + TILE / 2) as f64 - v.cz).abs());
                                (d, (tx, tz))
                            })
                            .collect();
                        by_dist.sort_by(
                            |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal),
                        );
                        let keep: std::collections::HashSet<(i32, i32)> = by_dist
                            .iter()
                            .take(MAX_TILES)
                            .map(|&(_, k)| k)
                            .collect();
                        tiles.write().retain(|k, _| keep.contains(k));
                    }
                }
                sleep_ms(100.0).await;
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

    // Redraw on any input change (tiles, agents, view, canvas size).
    use_effect(move || {
        let v = *view.read();
        let csize = *csize.read();
        let tiles_now = (*tiles.read()).clone();
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
        draw(&ctx, &canvas, &v, &tiles_now, &agents);
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
    // True when the visible rect exceeds the fetchable span cap, so the
    // tile mosaic covers only the middle of the canvas (honest zoom-out
    // beyond MAX_SPAN blocks).
    let (cw_px, ch_px) = *csize.read();
    let limited = cw_px as f64 / view_now.scale + 2.0 * MAP_MARGIN > (MAX_SPAN as f64)
        || ch_px as f64 / view_now.scale + 2.0 * MAP_MARGIN > (MAX_SPAN as f64);
    let hud = format!(
        "{}% · center ({}, {}){}",
        (view_now.scale * 100.0).round() as i32,
        view_now.cx.round() as i32,
        view_now.cz.round() as i32,
        if limited { " · max map extent" } else { "" },
    );

    rsx! {
        div { class: "app",
            header { class: "topbar",
                div { class: "title", "Qwencraft server" }
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
                                span { class: "pid", title: format!("player id {}", a.id), {a.label()} }
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
                            let v = *view.read();
                            // 1:1 pan: pixels moved / px-per-block = blocks.
                            *view.write() = View {
                                cx: v.cx - dx / v.scale,
                                cz: v.cz - dy / v.scale,
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
                            // Normalise delta to pixels (mice mostly report
                            // pixels; lines/pages are converted).
                            let (dx, dy) = match e.delta() {
                                dioxus::html::geometry::WheelDelta::Pixels(v) => (v.x, v.y),
                                dioxus::html::geometry::WheelDelta::Lines(v) => {
                                    (v.x * 16.0, v.y * 16.0)
                                }
                                dioxus::html::geometry::WheelDelta::Pages(v) => {
                                    (v.x * 160.0, v.y * 160.0)
                                }
                            };
                            let v = *view.read();
                            // Trackpad pinch (Chrome reports it as ctrl+wheel)
                            // and classic mouse-wheel notches (discrete
                            // ±100 px steps) zoom, exponentially and anchored
                            // at the cursor; everything else — two-finger
                            // trackpad scroll — pans 1:1, so an accidental
                            // left swipe pans the map instead of flying the
                            // zoom (or navigating the browser).
                            let pinch = e.modifiers().contains(Modifiers::CONTROL);
                            let zoom = pinch || (dx == 0.0 && dy.abs() >= 40.0);
                            if zoom {
                                let p = e.element_coordinates();
                                let k = if pinch { 0.012 } else { 0.005 };
                                *view.write() = zoom_around(
                                    v,
                                    p.x.max(0.0),
                                    p.y.max(0.0),
                                    canvas.width() as f64,
                                    canvas.height() as f64,
                                    (-dy * k).exp(),
                                );
                            } else {
                                *view.write() = View {
                                    cx: v.cx - dx / v.scale,
                                    cz: v.cz - dy / v.scale,
                                    scale: v.scale,
                                };
                            }
                        },
                    }
                    div { class: "map-hud", { hud }
                        span { class: "hint", "drag / scroll to pan · pinch / wheel to zoom" }
                    }
                }
            }
        }
    }
}

/// The JS entry point (called from index.html after `init()`):
/// `import init, { start } from "./qwencraft-dashboard.js"; init().then(start);`
///
/// A cdylib with an explicit `#[wasm_bindgen]` entry is used instead of a
/// binary's `main`: the wasm-bindgen glue only re-exports `#[wasm_bindgen]`
/// functions, and a binary's `main` (which takes argv) is not one.
#[wasm_bindgen]
pub fn start() {
    console_error_panic_hook::set_once();
    launch(App);
}
