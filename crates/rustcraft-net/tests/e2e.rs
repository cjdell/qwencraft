//! End-to-end test: a real (synchronous) WebSocket client against the
//! headless server. Exercises the full loop: handshake, Hello, world
//! streaming, input -> movement, block edits, and the NPC load dial.
//!
//! Runs on a multi-threaded runtime: the client below blocks on socket
//! reads while the server task ticks in the background.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use rustcraft_net::{serve, ServerOptions};
use rustcraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use rustcraft_server::{Key, KeySet, Vec3};
use rustcraft_world::{BlockPos, ChunkPos};
use futures_util::StreamExt;
use tungstenite::{connect, Message};

const SEED: u64 = 1337;

type Sock = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Read and fold server messages until `secs` have elapsed.
struct Sample {
    player: Option<rustcraft_server::AgentState>,
    chunks: u32,
    chunk_positions: Vec<ChunkPos>,
    npcs: u32,
    /// Max number of player agents seen in an `Agents` message (a shared
    /// world with N connected clients reports N players to each of them).
    players: u32,
    /// Player display names seen in `Agents` messages (shared world: each
    /// client sees every player's name).
    player_names: Vec<String>,
    npc_load: Option<(u32, f32)>,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            player: None,
            chunks: 0,
            chunk_positions: Vec::new(),
            npcs: 0,
            players: 0,
            player_names: Vec::new(),
            npc_load: None,
        }
    }
}

fn sample(sock: &mut Sock, secs: f32) -> Sample {
    let start = Instant::now();
    let mut s = Sample::default();
    while start.elapsed().as_secs_f32() < secs {
        match sock.read() {
            Ok(Message::Binary(data)) => {
                for m in ServerMsg::decode_stream(&data).0 {
                    match m {
                        ServerMsg::PlayerState(p) => s.player = Some(p),
                        ServerMsg::Chunk { pos, .. } => {
                            s.chunks += 1;
                            s.chunk_positions.push(pos);
                        }
                        ServerMsg::Stats(st) => s.npcs = st.npcs as u32,
                        ServerMsg::NpcLoad { count, spacing } => {
                            s.npc_load = Some((count, spacing));
                        }
                        ServerMsg::Agents(v) => {
                            let np = v.iter().filter(|x| x.is_player).count() as u32;
                            s.players = s.players.max(np);
                            for a in v.iter().filter(|x| x.is_player) {
                                if !s.player_names.contains(&a.name) {
                                    s.player_names.push(a.name.clone());
                                }
                            }
                        }
                        ServerMsg::Hello { .. } => {}
                    }
                }
            }
            Ok(Message::Ping(_)) => {
                // tungstenite already auto-ponged; nothing to do.
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    s
}

fn send(sock: &mut Sock, msg: &ClientMsg) {
    sock.send(Message::Binary(msg.encode().into()))
        .expect("send");
}

/// First frame must be the binary Hello; return the player id it carries
/// (the client uses it to skip rendering its own sphere).
fn expect_hello(sock: &mut Sock, name: &str) -> u32 {
    let hello = match sock.read().expect("read") {
        Message::Binary(data) => ServerMsg::decode_stream(&data).0,
        other => panic!("first frame must be binary, got {other:?}"),
    };
    match hello.into_iter().next().expect("hello present") {
        ServerMsg::Hello { version, seed, player_id } => {
            assert_eq!(version, PROTOCOL_VERSION, "protocol version on {name}");
            assert_eq!(seed, SEED, "seed on {name}");
            player_id
        }
        other => panic!("first message must be Hello, got {other:?}"),
    }
}

fn dist(a: Vec3, b: Vec3) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// The chunk containing the block under the player's feet.
fn feet_chunk(p: &rustcraft_server::AgentState) -> ChunkPos {
    ChunkPos::of(BlockPos::new(
        p.pos.x as i32,
        p.pos.y as i32 - 1,
        p.pos.z as i32,
    ))
}

/// Break the block under the feet (aim straight down: always a hit while on
/// the ground — a shallow aim can legitimately miss over a slope, and the
/// server raycasts exactly the stamped aim).
fn break_under_feet(sock: &mut Sock, p: &rustcraft_server::AgentState) {
    send(
        sock,
        &ClientMsg::Action(rustcraft_server::Action::Break {
            yaw: p.yaw,
            pitch: -1.55,
        }),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_single_player() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0, // single port: /ws + /dashboard + game client
        ..Default::default()
    })
    .await
    .expect("serve");
    let addr = ep.addr;
    let (mut sock, _) = connect(&format!("ws://{addr}/ws")).expect("connect");

    // 1) Hello with our protocol version and seed.
    let player_id = expect_hello(&mut sock, "conn");
    assert_eq!(player_id, 0, "first connection is player 0");

    // 2) The world streams in: at least one chunk and a live player state.
    let s = sample(&mut sock, 2.0);
    assert!(s.chunks >= 1, "no chunk regions streamed");
    let p0 = s.player.expect("no player state");
    assert!(p0.is_player);
    assert!(
        (0.0..60.0).contains(&p0.pos.y),
        "spawn height out of range: {:?}",
        p0.pos
    );

    // 3) Hold W: the player walks (server physics driven by our input).
    // The key is level-triggered: one Input snapshot is applied every tick
    // until it is replaced.
    let mut keys = KeySet::default();
    keys.insert(Key::W);
    send(&mut sock, &ClientMsg::Input { keys: keys.bits(), dx: 0.0, dy: 0.0 });
    let s = sample(&mut sock, 1.5);
    let p = s.player.expect("no player state after walking");
    let moved = dist(p.pos, p0.pos);
    assert!(
        moved > 0.5,
        "player should have walked while holding W (moved {moved} blocks)"
    );

    // Release the key (level-triggered input must clear) and wait until the
    // player stands still, so the break below lands on a standing player
    // (if the walk ended on a ledge, let the fall finish first).
    send(&mut sock, &ClientMsg::Input { keys: 0, dx: 0.0, dy: 0.0 });
    let mut p = sample(&mut sock, 0.25)
        .player
        .expect("no player state after release");
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) && !p.on_ground {
        let s = sample(&mut sock, 0.25);
        if let Some(p1) = s.player {
            p = p1;
        }
    }
    assert!(p.on_ground, "player should be standing before the break");

    // 4) Break the block under the feet. The edit lands in an already
    // generated chunk (written straight into the buffer, not the pending
    // delta layer), so the wire-visible effect is a re-send of the edited
    // chunk's region — wait for exactly that chunk to come back.
    let edited = feet_chunk(&p);
    break_under_feet(&mut sock, &p);
    let s = sample(&mut sock, 3.0);
    assert!(
        s.chunk_positions.contains(&edited),
        "edited chunk region must be re-sent after break (got {} chunks, positions {:?})",
        s.chunks,
        s.chunk_positions
    );

    // 5) The NPC load dial: set it and watch it take effect.
    send(&mut sock, &ClientMsg::SetNpcLoad { count: 8, spacing: 8.0 });
    let s = sample(&mut sock, 1.5);
    assert_eq!(s.npc_load, Some((8, 8.0)), "server should echo the load dial");
    assert_eq!(s.npcs, 8, "exactly 8 NPCs after SetNpcLoad");

    // 6) Pool eviction: report the player's own chunk as evicted — the
    // streamer must re-send it (it is still visible), so the client can
    // rebuild it. This is what keeps walked-back-over terrain from turning
    // into holes.
    let p = s.player.expect("player state");
    let evicted = feet_chunk(&p);
    send(&mut sock, &ClientMsg::Evicted(vec![evicted]));
    let s = sample(&mut sock, 2.0);
    assert!(
        s.chunk_positions.contains(&evicted),
        "evicted, still-visible chunk must be re-sent by the stream (got {} chunks)",
        s.chunks
    );

    drop(sock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_connections_share_one_world() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0, // single port: /ws + /dashboard + game client
        ..Default::default()
    })
    .await
    .expect("serve");
    let url = format!("ws://{}/ws", ep.addr);
    let (mut a, _) = connect(&url).expect("connect A");
    let (mut b, _) = connect(&url).expect("connect B");

    let pid_a = expect_hello(&mut a, "A");
    let pid_b = expect_hello(&mut b, "B");
    assert_ne!(pid_a, pid_b, "each connection gets its own player id");

    // Let both players' views stream in.
    let sa = sample(&mut a, 4.0);
    let sb = sample(&mut b, 4.0);
    assert!(sa.chunks >= 1 && sb.chunks >= 1, "both views must stream");

    // Shared world: each connection's `Agents` list contains BOTH players
    // (itself and the other), so each client can see the other.
    assert!(
        sa.players >= 2,
        "A must see both players in the shared world (saw {})",
        sa.players
    );
    assert!(
        sb.players >= 2,
        "B must see both players in the shared world (saw {})",
        sb.players
    );

    // A breaks the block under its feet; the edit lands in the shared world,
    // so B (which holds that chunk) must receive the region re-send. This is
    // what makes one player's block edits visible to the other.
    let pa = sample(&mut a, 0.3).player.expect("A player");
    let edited = feet_chunk(&pa);
    break_under_feet(&mut a, &pa);
    let sb2 = sample(&mut b, 3.0);
    assert!(
        sb2.chunk_positions.contains(&edited),
        "B must receive A's edited chunk (shared world) — got {} chunks",
        sb2.chunks
    );

    // A announces a profile (name + sphere colour); B must see it in the
    // agent list — this is what lets players see each other (sphere +
    // name tag).
    send(
        &mut a,
        &ClientMsg::Profile {
            name: "Alice".to_string(),
            color: [10, 200, 255],
        },
    );
    let sb3 = sample(&mut b, 1.5);
    assert!(
        sb3.player_names.contains(&"Alice".to_string()),
        "B must see A's name in the shared agent list (saw {:?})",
        sb3.player_names
    );

    drop(a);
    drop(b);
}

/// End-to-end over wss:// with a self-signed cert generated by openssl (the
/// nix shell provides it; the test skips gracefully elsewhere). The client
/// trusts exactly that cert and does a full Hello round-trip over TLS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_serves_encrypted_sessions() {
    let Some(openssl) = which("openssl") else {
        eprintln!("note: openssl not found; skipping wss test");
        return;
    };
    let dir =
        std::env::temp_dir().join(format!("rustcraft-net-wss-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let status = std::process::Command::new(&openssl)
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            cert.to_str().unwrap(),
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            // `req -x509` defaults to CA:TRUE, which webpki rejects as an
            // end-entity cert; mark it as a leaf.
            "-addext",
            "basicConstraints=critical,CA:FALSE",
        ])
        .status()
        .expect("spawn openssl");
    assert!(status.success(), "openssl could not generate a self-signed cert");

    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        bind: "127.0.0.1".parse().unwrap(),
        cert: Some(cert.clone()),
        key: Some(key.clone()),
    })
    .await
    .expect("serve wss");
    let addr = ep.addr;

    // Client trusting only the self-signed cert above.
    let cert_pem = std::fs::read_to_string(&cert).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    let (valid, _invalid) = roots.add_parsable_certificates(
        rustls_pemfile::certs(&mut cert_pem.as_bytes()).flatten(),
    );
    assert_eq!(valid, 1, "self-signed cert must parse");
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let tcp = tokio::net::TcpStream::connect(addr).await.expect("tcp connect");
    let (mut ws, _resp) = tokio_tungstenite::client_async_tls_with_config(
        &format!("wss://{addr}/ws"),
        tcp,
        None,
        Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(cfg))),
    )
    .await
    .expect("wss handshake");

    let hello = match ws.next().await.unwrap().expect("message") {
        Message::Binary(data) => ServerMsg::decode_stream(&data).0,
        other => panic!("first frame must be binary, got {other:?}"),
    };
    match hello.into_iter().next().expect("hello present") {
        ServerMsg::Hello { version, seed, player_id } => {
            assert_eq!(version, PROTOCOL_VERSION);
            assert_eq!(seed, SEED);
            assert_eq!(player_id, 0);
        }
        other => panic!("first message must be Hello, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

fn which(bin: &str) -> Option<String> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file().then(|| candidate.to_string_lossy().into_owned())
        })
    })
}

/// Minimal synchronous HTTP GET against the dashboard server (the server
/// answers `Connection: close`, so read-to-EOF gives the full response).
/// Returns (status code, content-type, body).
async fn http_get(addr: &SocketAddr, target: &str) -> (u16, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("http connect");
    let req = format!(
        "GET {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    );
    tcp.write_all(req.as_bytes())
        .await
        .expect("http write");
    let mut buf = Vec::new();
    tcp.read_to_end(&mut buf).await.expect("http read");
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head terminator");
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let body = buf[head_end + 4..].to_vec();
    let mut lines = head.lines();
    let code: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut ctype = String::new();
    for l in lines {
        if let Some(v) = l.strip_prefix("Content-Type: ") {
            ctype = v.trim().to_string();
        }
    }
    (code, ctype, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_http_serves_status_map_and_assets() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0, // single port: /ws + /dashboard + game client
        ..Default::default()
    })
    .await
    .expect("serve");
    let addr = ep.addr;

    // Health probe.
    let (code, _ct, body) = http_get(&addr, "/healthz").await;
    assert_eq!(code, 200);
    assert_eq!(body, b"ok");

    // Status before any client: no players, seed present, startup logged.
    let (code, ct, body) = http_get(&addr, "/api/status").await;
    assert_eq!(code, 200);
    assert!(ct.contains("application/json"), "{ct}");
    let s = String::from_utf8_lossy(&body).into_owned();
    assert!(s.contains(&format!("\"seed\":{SEED}")), "status: {s}");
    assert!(s.contains("\"players\":0"), "status: {s}");
    assert!(s.contains("server started"), "event log: {s}");

    // The dashboard page + its assets (the embedded dioxus build), under
    // the /dashboard path on the same port as the WebSocket.
    let (code, ct, body) = http_get(&addr, "/dashboard/").await;
    assert_eq!(code, 200);
    assert!(ct.contains("text/html"));
    assert!(String::from_utf8_lossy(&body).contains("RustCraft"));
    let (code, ct, _) = http_get(&addr, "/dashboard/rustcraft_dashboard.js").await;
    assert_eq!(code, 200);
    assert!(ct.contains("javascript"), "{ct}");
    let (code, ct, body) = http_get(&addr, "/dashboard/rustcraft_dashboard_bg.wasm").await;
    assert_eq!(code, 200);
    assert_eq!(ct, "application/wasm");
    assert!(body.len() > 10_000, "wasm asset looks empty: {} bytes", body.len());
    let (code, _ct, _) = http_get(&addr, "/dashboard/dashboard.css").await;
    assert_eq!(code, 200);

    // The root serves the game client (web/dist when built) or the fallback
    // page — either way, an HTML page mentioning RustCraft.
    let (code, ct, body) = http_get(&addr, "/").await;
    assert_eq!(code, 200);
    assert!(ct.contains("text/html"), "{ct}");
    assert!(String::from_utf8_lossy(&body).contains("RustCraft"), "root page: {body:?}");

    // /ws over plain HTTP answers an upgrade-required error (the WebSocket
    // handshake itself is covered by the ws tests).
    let (code, _, _) = http_get(&addr, "/ws").await;
    assert_eq!(code, 426);

    // A player joins over ws: the status must reflect it (agent + event).
    let (mut sock, _) = connect(&format!("ws://{addr}/ws")).expect("connect");
    let pid = expect_hello(&mut sock, "conn");
    // Announce a profile: the status JSON must carry the player's name.
    send(&mut sock, &ClientMsg::Profile {
        name: "MapReader".to_string(),
        color: [9, 9, 9],
    });
    let _ = sample(&mut sock, 0.5);
    let (_, _, body) = http_get(&addr, "/api/status").await;
    let s = String::from_utf8_lossy(&body).into_owned();
    assert!(s.contains("\"players\":1"), "status: {s}");
    assert!(s.contains("\"player\":true"), "agent present: {s}");
    assert!(s.contains(&format!("\"name\":\"MapReader\"")), "player name: {s}");
    assert!(s.contains(&format!("player {pid} joined")), "join event: {s}");

    // Map: 64x64 region = 64*64*2 bytes, mostly non-air near spawn, and
    // the origin header echoes the clamped region.
    let (code, _ct, body) = http_get(&addr, "/api/map?x=8&z=8&w=64&h=64").await;
    assert_eq!(code, 200);
    assert_eq!(body.len(), 64 * 64 * 2);
    let mut non_air = 0usize;
    let mut i = 0;
    while i < body.len() {
        if body[i + 1] != 0 {
            non_air += 1;
        }
        i += 2;
    }
    assert!(
        non_air > 64 * 64 / 2,
        "map near spawn should be mostly non-air (got {non_air})"
    );

    // Oversized requests clamp to the max side (256), tiny ones to the min.
    let (_, _, body) = http_get(&addr, "/api/map?x=8&z=8&w=4096&h=4096").await;
    assert_eq!(body.len(), 256 * 256 * 2);
    let (_, _, body) = http_get(&addr, "/api/map?x=8&z=8&w=1&h=1").await;
    assert_eq!(body.len(), 16 * 16 * 2);

    // Unknown paths 404.
    let (code, _, _) = http_get(&addr, "/nope").await;
    assert_eq!(code, 404);

    drop(sock);
}
