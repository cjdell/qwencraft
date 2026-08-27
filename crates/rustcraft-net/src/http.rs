//! Minimal HTTP/1.1 front end for the server dashboard (only `include_dir`
//! as a dependency): serves the embedded dioxus dashboard build (the whole
//! `dashboard/dist` tree — the app's JS imports a `snippets/` subtree) plus
//! two status endpoints:
//!
//! - `GET /api/status` → JSON: seed, uptime, agents (players + NPCs),
//!   event log;
//! - `GET /api/map?x=&z=&w=&h=` → binary: topmost block per column for a
//!   region (2 bytes/column, row-major; see `crate::map`).
//!
//! Deliberately small: GET only, one request per connection
//! (`Connection: close`), bounded request reads. It is a LAN debugging
//! tool, not a public web server.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use include_dir::Dir;
use include_dir::include_dir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::map::MapState;
use crate::WorldState;

/// The dioxus dashboard build output (`scripts/build_dashboard.sh`),
/// embedded so the server has no filesystem dependencies at runtime. The
/// built assets are committed; rerun the script (and this crate) after
/// changing the dashboard sources.
static DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../dashboard/dist");

/// Bind the HTTP listener and spawn the accept loop. Returns the bound
/// address.
pub async fn run_http(
    bind: IpAddr,
    port: u16,
    world: Arc<Mutex<WorldState>>,
    map: Arc<Mutex<MapState>>,
) -> Result<SocketAddr, String> {
    let listener = TcpListener::bind((bind, port))
        .await
        .map_err(|e| format!("bind {bind}:{port} (dashboard http): {e}"))?;
    let addr = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
    eprintln!("[rustcraft-net] dashboard on http://{addr}");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp, peer)) => {
                    let world = world.clone();
                    let map = map.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(tcp, world, map).await {
                            eprintln!("[rustcraft-net] {peer}: http: {e}");
                        }
                    });
                }
                Err(e) => eprintln!("[rustcraft-net] http accept failed: {e}"),
            }
        }
    });
    Ok(addr)
}

/// One request per connection (we always reply `Connection: close`).
async fn handle_conn(
    mut tcp: TcpStream,
    world: Arc<Mutex<WorldState>>,
    map: Arc<Mutex<MapState>>,
) -> Result<(), String> {
    let mut buf = [0u8; 8192];
    let n = tcp.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let (first, query) = match req.lines().next().and_then(|l| l.split_whitespace().nth(1)) {
        Some(p) => {
            let (path, q) = match p.split_once('?') {
                Some((p, q)) => (p, Some(q.to_string())),
                None => (p, None),
            };
            (path, q)
        }
        None => {
            respond(&mut tcp, 400, "text/plain; charset=utf-8", b"bad request").await?;
            return Ok(());
        }
    };
    let (status, body, content_type) = route(first, query.as_deref(), &world, &map).await;
    respond(&mut tcp, status, content_type, &body).await
}

/// Route a request: API endpoints, then the embedded dashboard assets.
async fn route(
    path: &str,
    query: Option<&str>,
    world: &Arc<Mutex<WorldState>>,
    map: &Arc<Mutex<MapState>>,
) -> (u16, Vec<u8>, &'static str) {
    match path {
        "/healthz" => (200, b"ok".to_vec(), "text/plain; charset=utf-8"),
        "/api/status" => {
            let w = world.lock().unwrap();
            (
                200,
                status_json(&w).into_bytes(),
                "application/json; charset=utf-8",
            )
        }
        "/api/map" => match parse_map_query(query) {
            Ok((cx, cz, w, h)) => {
                let mut m = map.lock().unwrap();
                // `top_map` clamps the side to MAP_MIN..=MAP_MAX itself.
                let region = m.top_map(cx, cz, w, h);
                // Flat 2-bytes-per-column payload ([y, block id], row-major).
                (200, region.cols, "application/octet-stream")
            }
            Err(()) => (400, b"bad ?x=&z=&w=&h=".to_vec(), "text/plain; charset=utf-8"),
        },
        _ => serve_static(path),
    }
}

/// Look up `path` in the embedded dashboard build.
fn serve_static(path: &str) -> (u16, Vec<u8>, &'static str) {
    let rel = path.strip_prefix('/').unwrap_or(path);
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let Some(file) = DIST.get_file(rel) else {
        return (404, b"not found".to_vec(), "text/plain; charset=utf-8");
    };
    let mime = match rel.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    };
    (
        200,
        file.contents().to_vec(),
        mime,
    )
}

/// `?x=&z=&w=&h=` → (centre x, centre z, width, height) in blocks.
fn parse_map_query(query: Option<&str>) -> Result<(i32, i32, i32, i32), ()> {
    let mut x: Option<i32> = None;
    let mut z: Option<i32> = None;
    let mut w: Option<i32> = None;
    let mut h: Option<i32> = None;
    for kv in query.unwrap_or_default().split('&') {
        let (k, v) = match kv.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        match k {
            "x" => x = v.parse().ok(),
            "z" => z = v.parse().ok(),
            "w" => w = v.parse().ok(),
            "h" => h = v.parse().ok(),
            _ => {}
        }
    }
    Ok((
        x.unwrap_or(0),
        z.unwrap_or(0),
        w.unwrap_or(64),
        h.unwrap_or(64),
    ))
}
/// Build the `/api/status` JSON payload (the dashboard's `Status` shape:
/// flat agents, `{t, m}` events). Plain string building — no `format!` —
/// so the JSON braces need no format-string escaping.
fn status_json(w: &WorldState) -> String {
    let agents = w.server.agents();
    let players = agents.iter().filter(|a| a.is_player).count();
    let npcs = agents.len() - players;
    let mut json = String::new();
    json.push_str("{\"seed\":");
    json.push_str(&w.server.seed().to_string());
    json.push_str(",\"uptime\":");
    json.push_str(&w.started.elapsed().as_secs_f64().to_string());
    json.push_str(",\"players\":");
    json.push_str(&players.to_string());
    json.push_str(",\"npcs\":");
    json.push_str(&npcs.to_string());
    json.push_str(",\"agents\":[");
    for (i, a) in agents.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str("{\"id\":");
        json.push_str(&a.id.to_string());
        json.push_str(",\"player\":");
        json.push_str(if a.is_player { "true" } else { "false" });
        json.push_str(",\"x\":");
        json.push_str(&a.pos.x.to_string());
        json.push_str(",\"y\":");
        json.push_str(&a.pos.y.to_string());
        json.push_str(",\"z\":");
        json.push_str(&a.pos.z.to_string());
        json.push_str(",\"yaw\":");
        json.push_str(&a.yaw.to_string());
        json.push_str(",\"fly\":");
        json.push_str(if a.fly { "true" } else { "false" });
        json.push_str(",\"ground\":");
        json.push_str(if a.on_ground { "true" } else { "false" });
        json.push_str(",\"color\":[");
        json.push_str(&a.color[0].to_string());
        json.push(',');
        json.push_str(&a.color[1].to_string());
        json.push(',');
        json.push_str(&a.color[2].to_string());
        json.push_str("]}");
    }
    json.push_str("],\"events\":[");
    for (i, (t, m)) in w.events.snapshot().iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str("{\"t\":");
        json.push_str(&t.to_string());
        json.push_str(",\"m\":");
        json.push_str(&json_escape(m));
        json.push('}');
    }
    json.push_str("]}");
    json
}

/// Minimal JSON string escaping (control chars, quote, backslash).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

async fn respond(
    tcp: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nAccess-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(head.as_bytes())
        .await
        .map_err(|e| format!("write head: {e}"))?;
    tcp.write_all(body)
        .await
        .map_err(|e| format!("write body: {e}"))?;
    tcp.flush().await.map_err(|e| format!("flush: {e}"))
}
