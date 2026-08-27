//! End-to-end test: a real (synchronous) WebSocket client against the
//! headless server. Exercises the full loop: handshake, Hello, world
//! streaming, input -> movement, block edits, and the NPC load dial.
//!
//! Runs on a multi-threaded runtime: the client below blocks on socket
//! reads while the server task ticks in the background.

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
    npc_load: Option<(u32, f32)>,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            player: None,
            chunks: 0,
            chunk_positions: Vec::new(),
            npcs: 0,
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
                        ServerMsg::Hello { .. } | ServerMsg::Agents(_) => {}
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

/// First frame must be the binary Hello; return it.
fn expect_hello(sock: &mut Sock, name: &str) {
    let hello = match sock.read().expect("read") {
        Message::Binary(data) => ServerMsg::decode_stream(&data).0,
        other => panic!("first frame must be binary, got {other:?}"),
    };
    match hello.into_iter().next().expect("hello present") {
        ServerMsg::Hello { version, seed } => {
            assert_eq!(version, PROTOCOL_VERSION, "protocol version on {name}");
            assert_eq!(seed, SEED, "seed on {name}");
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
    let addr = serve(ServerOptions {
        seed: SEED,
        port: 0,
        ..Default::default()
    })
    .await
    .expect("serve");
    let (mut sock, _) = connect(&format!("ws://{addr}")).expect("connect");

    // 1) Hello with our protocol version and seed.
    expect_hello(&mut sock, "conn");

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

    // 6) Resend: ask for the player's own chunk region; a chunk must come back.
    let p = s.player.expect("player state");
    send(&mut sock, &ClientMsg::ResendChunk(feet_chunk(&p)));
    let s = sample(&mut sock, 1.0);
    assert!(s.chunks >= 1, "re-send of a generated chunk must arrive");

    drop(sock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_connections_get_independent_worlds() {
    let addr = serve(ServerOptions {
        seed: SEED,
        port: 0,
        ..Default::default()
    })
    .await
    .expect("serve");
    let url = format!("ws://{addr}");
    let (mut a, _) = connect(&url).expect("connect A");
    let (mut b, _) = connect(&url).expect("connect B");

    expect_hello(&mut a, "A");
    expect_hello(&mut b, "B");

    // Both spawn in the same place (same seed), so let their worlds finish
    // streaming, then verify a quiet window: no chunk traffic when idle.
    sample(&mut a, 4.0);
    sample(&mut b, 4.0);
    let qa = sample(&mut a, 1.0);
    let qb = sample(&mut b, 1.0);
    assert_eq!(qa.chunks, 0, "A should be fully streamed (quiet window)");
    assert_eq!(qb.chunks, 0, "B should be fully streamed (quiet window)");

    // Edit the world on A only: A's edited chunk must be re-sent, and B's
    // world must stay silent (each connection owns its world — the
    // single-player model).
    let pa = sample(&mut a, 0.2).player.expect("A player");
    let edited = feet_chunk(&pa);
    break_under_feet(&mut a, &pa);
    let sa = sample(&mut a, 3.0);
    assert!(
        sa.chunk_positions.contains(&edited),
        "A's edited chunk must be re-sent in A's world"
    );
    let sb = sample(&mut b, 0.5);
    assert_eq!(
        sb.chunks, 0,
        "B's world must not see A's edits (no chunk traffic expected)"
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

    let addr = serve(ServerOptions {
        seed: SEED,
        port: 0,
        bind: "127.0.0.1".parse().unwrap(),
        cert: Some(cert.clone()),
        key: Some(key.clone()),
    })
    .await
    .expect("serve wss");

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
        &format!("wss://{addr}"),
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
        ServerMsg::Hello { version, seed } => {
            assert_eq!(version, PROTOCOL_VERSION);
            assert_eq!(seed, SEED);
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
