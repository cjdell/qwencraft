//! End-to-end test: a real (synchronous) WebSocket client against the
//! headless server. Exercises the full loop: handshake, Hello, world
//! streaming, input -> movement, block edits, and the NPC load dial.
//!
//! Runs on a multi-threaded runtime: the client below blocks on socket
//! reads while the server task ticks in the background.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use qwencraft_net::{serve, ServerOptions};
use qwencraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use qwencraft_server::{Key, KeySet, Vec3};
use qwencraft_world::{BlockPos, ChunkPos};
use futures_util::StreamExt;
use tungstenite::{connect, Message};

const SEED: u64 = 1337;

type Sock = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Read and fold server messages until `secs` have elapsed.
struct Sample {
    player: Option<qwencraft_server::AgentState>,
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
    /// Console `getBlock` answers (position, block id), in arrival order.
    block_ats: Vec<(BlockPos, u8)>,
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
            block_ats: Vec::new(),
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
                        ServerMsg::BlockAt { pos, block } => s.block_ats.push((pos, block)),
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

/// A throwaway data directory for one test: the server's world save lives
/// here, and tests must never share a save (parallel tests + restart
/// round-trips). Fresh each call (tag + pid + nanos is unique).
fn test_data_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qwencraft-net-test-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}


/// First frame must be the binary Hello; return the player id it carries
/// (the client uses it to skip rendering its own sphere) and the
/// connection's rejoin token (minted for fresh identities, re-issued for
/// recognised rejoiners).
fn expect_hello(sock: &mut Sock, name: &str) -> (u32, [u8; 16]) {
    let hello = match sock.read().expect("read") {
        Message::Binary(data) => ServerMsg::decode_stream(&data).0,
        other => panic!("first frame must be binary, got {other:?}"),
    };
    match hello.into_iter().next().expect("hello present") {
        ServerMsg::Hello { version, seed, player_id, token } => {
            assert_eq!(version, PROTOCOL_VERSION, "protocol version on {name}");
            assert_eq!(seed, SEED, "seed on {name}");
            (player_id, token)
        }
        other => panic!("first message must be Hello, got {other:?}"),
    }
}

/// Join the shared world the way the v8 client does: send the first-frame
/// identity claim (all-zero token = fresh identity), then read the Hello.
fn join(sock: &mut Sock, name: &str) -> (u32, [u8; 16]) {
    send(sock, &ClientMsg::Rejoin { token: [0u8; 16] });
    expect_hello(sock, name)
}

/// Poll the dashboard event log until `needle` appears (5 s deadline): a
/// deterministic sync point after a disconnect (the session cleanup — and
/// the rejoin record it writes — have run).
async fn wait_for_event(addr: &SocketAddr, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, _, _, body) = http_get(addr, "/api/status").await;
        if String::from_utf8_lossy(&body).contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "event {needle:?} never appeared in the log"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn dist(a: Vec3, b: Vec3) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// The chunk containing the block under the player's feet.
fn feet_chunk(p: &qwencraft_server::AgentState) -> ChunkPos {
    ChunkPos::of(BlockPos::new(
        p.pos.x as i32,
        p.pos.y as i32 - 1,
        p.pos.z as i32,
    ))
}

/// Break the block under the feet (aim straight down: always a hit while on
/// the ground — a shallow aim can legitimately miss over a slope, and the
/// server raycasts exactly the stamped aim).
fn break_under_feet(sock: &mut Sock, p: &qwencraft_server::AgentState) {
    send(
        sock,
        &ClientMsg::Action(qwencraft_server::Action::Break {
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
        data_dir: test_data_dir("single"),
        ..Default::default()
    })
    .await
    .expect("serve");
    let addr = ep.addr;
    let (mut sock, _) = connect(&format!("ws://{addr}/ws")).expect("connect");

    // 1) Hello with our protocol version and seed.
    let (player_id, token) = join(&mut sock, "conn");
    assert_ne!(token, [0u8; 16], "a fresh identity must be issued a token");
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
    send(
        &mut sock,
        &ClientMsg::Input { keys: keys.bits(), dx: 0.0, dy: 0.0, analog_x: 0.0, analog_y: 0.0 },
    );
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
    send(
        &mut sock,
        &ClientMsg::Input { keys: 0, dx: 0.0, dy: 0.0, analog_x: 0.0, analog_y: 0.0 },
    );
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

    // 4b) Place with a selected block (protocol v4: the action carries the
    // block id; the server validates it and re-sends the edited chunk).
    // Aim down-forward at the ground in front of the standing player.
    let placed_chunk = feet_chunk(&p);
    send(
        &mut sock,
        &ClientMsg::Action(qwencraft_server::Action::Place {
            yaw: p.yaw,
            pitch: -0.7,
            block: 11, // planks (a placeable id)
        }),
    );
    // 4c) An invalid block id must be ignored: no crash, no edit.
    send(
        &mut sock,
        &ClientMsg::Action(qwencraft_server::Action::Place {
            yaw: p.yaw,
            pitch: -0.7,
            block: 250,
        }),
    );
    let s = sample(&mut sock, 3.0);
    assert!(s.player.is_some(), "server must stay alive after an invalid place");
    assert!(
        s.chunk_positions.contains(&placed_chunk),
        "placed chunk region must be re-sent (got {} chunks)",
        s.chunks
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

/// Transit-loss reconciliation: if the client reports (via `Resync`) that
/// it only holds part of what the server sent, the server re-sends the
/// missing chunks — without this, a burst lost in flight (WAN flakiness,
/// early-connection stalls) stays a permanent hole until a block edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resync_resends_lost_chunks() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: test_data_dir("resync"),
        ..Default::default()
    })
    .await
    .expect("serve");
    let addr = ep.addr;
    let (mut sock, _) = connect(&format!("ws://{addr}/ws")).expect("connect");
    let _ = join(&mut sock, "resync");

    // Let the initial stream settle (the spawn view streams in ~2 s).
    let s = sample(&mut sock, 4.0);
    let positions: Vec<ChunkPos> = {
        let mut set: std::collections::HashSet<ChunkPos> = std::collections::HashSet::new();
        for p in s.chunk_positions {
            set.insert(p);
        }
        set.into_iter().collect()
    };
    assert!(
        positions.len() > 50,
        "the initial view must have streamed ({} chunks)",
        positions.len()
    );

    // Simulate transit loss: the client "kept" only the even-indexed
    // chunks and reports the survivors; the odd-indexed ones are the loss.
    let have: Vec<ChunkPos> = positions.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, p)| *p).collect();
    let lost: Vec<ChunkPos> = positions.iter().enumerate().filter(|(i, _)| i % 2 == 1).map(|(_, p)| *p).collect();
    send(&mut sock, &ClientMsg::Resync(have));

    // The server re-sends the missing set (repair path: uncapped); the
    // per-tick stream keeps the socket lively so this loop can't hang.
    let start = Instant::now();
    let mut got: std::collections::HashSet<ChunkPos> = std::collections::HashSet::new();
    while start.elapsed() < Duration::from_secs(10) && got.len() < lost.len() {
        match sock.read() {
            Ok(Message::Binary(data)) => {
                for m in ServerMsg::decode_stream(&data).0 {
                    if let ServerMsg::Chunk { pos, .. } = m {
                        got.insert(pos);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let missing: Vec<ChunkPos> = lost.iter().filter(|p| !got.contains(p)).copied().collect();
    assert!(
        missing.is_empty(),
        "resync must re-send every chunk the client reported missing ({} not re-sent, first few: {missing:?})",
        missing.len()
    );

    drop(sock);
}

/// Console API over the wire (protocol v6): `qwc.getBlock` round-trips to
/// the authoritative world, `qwc.setBlock` lands in the shared world (the
/// edited chunk re-sends), and `qwc.setPlayerPos` teleports the player.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_get_set_block_and_teleport() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: test_data_dir("console"),
        ..Default::default()
    })
    .await
    .expect("serve");
    let addr = ep.addr;
    let (mut sock, _) = connect(&format!("ws://{addr}/ws")).expect("connect");
    let (player_id, _token) = join(&mut sock, "console");
    assert_eq!(player_id, 0);

    // Let the spawn view settle; the player is standing (no input sent).
    let s = sample(&mut sock, 2.0);
    let p = s.player.expect("no player state");
    assert!(p.on_ground, "player should be standing for the block reads");

    // 1) GetBlock at the block under the feet: the answer must come back
    // as a BlockAt for exactly that position (non-air: standing on it).
    let under = BlockPos::new(p.pos.x as i32, p.pos.y as i32 - 1, p.pos.z as i32);
    send(&mut sock, &ClientMsg::GetBlock { pos: under });
    let s = sample(&mut sock, 2.0);
    let answer = s
        .block_ats
        .iter()
        .find(|(q, _)| *q == under)
        .expect("server must answer GetBlock with a BlockAt for that position");
    assert_ne!(answer.1, 0, "the block under a standing player must not be air");

    // 2) SetBlock: write stone a few blocks above the ground, then read it
    // back through the authoritative read (and the edited chunk region
    // re-sends, so the edit renders for this and every other viewer).
    let edit = BlockPos::new(under.x, under.y + 3, under.z);
    send(&mut sock, &ClientMsg::SetBlock { pos: edit, block: 3 }); // stone
    send(&mut sock, &ClientMsg::GetBlock { pos: edit });
    let s = sample(&mut sock, 3.0);
    assert_eq!(
        s.block_ats.iter().find(|(q, _)| *q == edit).map(|(_, b)| *b),
        Some(3),
        "the console edit must be readable back (block_ats: {:?})",
        s.block_ats
    );
    let edit_chunk = ChunkPos::of(edit);
    assert!(
        s.chunk_positions.contains(&edit_chunk),
        "the edited chunk region must re-send after the console edit"
    );
    // Out-of-world y is a no-op: the server stays alive and answers reads.
    send(
        &mut sock,
        &ClientMsg::SetBlock {
            pos: BlockPos::new(under.x, 9999, under.z),
            block: 3,
        },
    );
    let s = sample(&mut sock, 1.0);
    assert!(s.player.is_some(), "server must stay alive after a rejected edit");

    // 3) Teleport: the next PlayerState must be at the destination (no
    // input is flowing, so only the teleport moves the player).
    let target = Vec3::new(p.pos.x, p.pos.y, p.pos.z + 40.0);
    send(&mut sock, &ClientMsg::Teleport { pos: target });
    let s = sample(&mut sock, 1.5);
    let p2 = s.player.expect("no player state after teleport");
    assert!(
        (p2.pos.z - target.z).abs() < 3.0,
        "teleport should have moved the player to z≈{:.0} (was {:.0}, now {:.0})",
        target.z,
        p.pos.z,
        p2.pos.z
    );
    assert!(
        (p2.pos.x - target.x).abs() < 1.0,
        "teleport x must hold (no horizontal input)"
    );

    drop(sock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_connections_share_one_world() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0, // single port: /ws + /dashboard + game client
        data_dir: test_data_dir("shared"),
        ..Default::default()
    })
    .await
    .expect("serve");
    let url = format!("ws://{}/ws", ep.addr);
    let (mut a, _) = connect(&url).expect("connect A");
    let (mut b, _) = connect(&url).expect("connect B");

    let (pid_a, _tok_a) = join(&mut a, "A");
    let (pid_b, _tok_b) = join(&mut b, "B");
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
        std::env::temp_dir().join(format!("qwencraft-net-wss-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
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
        debug: false,
        data_dir: test_data_dir("wss"),
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

    // Identity handshake first (zero token = fresh identity).
    use futures_util::SinkExt;
    ws.send(Message::Binary(
        ClientMsg::Rejoin { token: [0u8; 16] }.encode().into(),
    ))
    .await
    .expect("send rejoin");

    let hello = match ws.next().await.unwrap().expect("message") {
        Message::Binary(data) => ServerMsg::decode_stream(&data).0,
        other => panic!("first frame must be binary, got {other:?}"),
    };
    match hello.into_iter().next().expect("hello present") {
        ServerMsg::Hello { version, seed, player_id, token } => {
            assert_eq!(version, PROTOCOL_VERSION);
            assert_eq!(seed, SEED);
            assert_eq!(player_id, 0);
            assert_ne!(token, [0u8; 16], "a fresh identity must get a token");
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
/// Returns (status code, content-type, location header, body).
async fn http_get(addr: &SocketAddr, target: &str) -> (u16, String, String, Vec<u8>) {
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
    let mut location = String::new();
    for l in lines {
        if let Some(v) = l.strip_prefix("Content-Type: ") {
            ctype = v.trim().to_string();
        }
        if let Some(v) = l.strip_prefix("Location: ") {
            location = v.trim().to_string();
        }
    }
    (code, ctype, location, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_http_serves_status_map_and_assets() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0, // single port: /ws + /dashboard + game client
        data_dir: test_data_dir("dashboard"),
        ..Default::default()
    })
    .await
    .expect("serve");
    let addr = ep.addr;

    // Health probe.
    let (code, _ct, _loc, body) = http_get(&addr, "/healthz").await;
    assert_eq!(code, 200);
    assert_eq!(body, b"ok");

    // Status before any client: no players, seed present, startup logged.
    let (code, ct, _loc, body) = http_get(&addr, "/api/status").await;
    assert_eq!(code, 200);
    assert!(ct.contains("application/json"), "{ct}");
    let s = String::from_utf8_lossy(&body).into_owned();
    assert!(s.contains(&format!("\"seed\":{SEED}")), "status: {s}");
    assert!(s.contains("\"players\":0"), "status: {s}");
    assert!(s.contains("server started"), "event log: {s}");

    // The dashboard page + its assets (the embedded dioxus build), under
    // the /dashboard path on the same port as the WebSocket.
    let (code, ct, _loc, body) = http_get(&addr, "/dashboard/").await;
    assert_eq!(code, 200);
    assert!(ct.contains("text/html"));
    assert!(String::from_utf8_lossy(&body).contains("Qwencraft"));
    // The bare /dashboard path redirects to the trailing-slash form: the
    // dashboard's index.html loads its assets with RELATIVE urls, so it
    // only resolves under /dashboard/ — the redirect keeps bare links
    // (and bookmarks) working.
    let (code, _ct, loc, _) = http_get(&addr, "/dashboard").await;
    assert_eq!(code, 302);
    assert_eq!(loc, "/dashboard/", "redirect target");
    let (code, ct, _loc, _) = http_get(&addr, "/dashboard/qwencraft_dashboard.js").await;
    assert_eq!(code, 200);
    assert!(ct.contains("javascript"), "{ct}");
    let (code, ct, _loc, body) = http_get(&addr, "/dashboard/qwencraft_dashboard_bg.wasm").await;
    assert_eq!(code, 200);
    assert_eq!(ct, "application/wasm");
    assert!(body.len() > 10_000, "wasm asset looks empty: {} bytes", body.len());
    let (code, _ct, _loc, _) = http_get(&addr, "/dashboard/dashboard.css").await;
    assert_eq!(code, 200);

    // The root serves the game client (web/dist when built) or the fallback
    // page — either way, an HTML page mentioning Qwencraft.
    let (code, ct, _loc, body) = http_get(&addr, "/").await;
    assert_eq!(code, 200);
    assert!(ct.contains("text/html"), "{ct}");
    assert!(String::from_utf8_lossy(&body).contains("Qwencraft"), "root page: {body:?}");

    // /ws over plain HTTP answers an upgrade-required error (the WebSocket
    // handshake itself is covered by the ws tests).
    let (code, _ct, _loc, _) = http_get(&addr, "/ws").await;
    assert_eq!(code, 426);

    // A player joins over ws: the status must reflect it (agent + event).
    let (mut sock, _) = connect(&format!("ws://{addr}/ws")).expect("connect");
    let (pid, _token) = join(&mut sock, "conn");
    // Announce a profile: the status JSON must carry the player's name.
    send(&mut sock, &ClientMsg::Profile {
        name: "MapReader".to_string(),
        color: [9, 9, 9],
    });
    let _ = sample(&mut sock, 0.5);
    let (_, _ct, _loc, body) = http_get(&addr, "/api/status").await;
    let s = String::from_utf8_lossy(&body).into_owned();
    assert!(s.contains("\"players\":1"), "status: {s}");
    assert!(s.contains("\"player\":true"), "agent present: {s}");
    assert!(s.contains(&format!("\"name\":\"MapReader\"")), "player name: {s}");
    assert!(s.contains(&format!("player {pid} joined")), "join event: {s}");

    // Map: 64x64 region = 64*64*2 bytes, mostly non-air near spawn, and
    // the origin header echoes the clamped region.
    let (code, _ct, _loc, body) = http_get(&addr, "/api/map?x=8&z=8&w=64&h=64").await;
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
    let (_c, _ct, _loc, body) = http_get(&addr, "/api/map?x=8&z=8&w=4096&h=4096").await;
    assert_eq!(body.len(), 256 * 256 * 2);
    let (_c, _ct, _loc, body) = http_get(&addr, "/api/map?x=8&z=8&w=1&h=1").await;
    assert_eq!(body.len(), 16 * 16 * 2);

    // Unknown paths 404.
    let (code, _ct, _loc, _) = http_get(&addr, "/nope").await;
    assert_eq!(code, 404);

    drop(sock);
}

/// World persistence end to end: edits made in one server process must
/// survive a restart into a fresh process (same data dir). The save file is
/// the seed + the world's block overrides (terrain is a pure function of
/// the seed), so a restarted server replays the edits onto the same terrain
/// and answers authoritative reads from them. A clean stop (`stop()`) flushes
/// the final save; a seed mismatch must fail fast instead of loading a
/// foreign world.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn world_edits_survive_restart() {
    use qwencraft_server::save;

    let data_dir = test_data_dir("restart");
    let save_path = data_dir.join(save::SAVE_FILE_NAME);

    // Session 1: connect, make two deterministic console edits, and read
    // them back (proving they are live before the restart).
    let edit_a = BlockPos::new(20, 40, 20);
    let edit_b = BlockPos::new(21, 40, 20);
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect("serve session 1");
    let (mut sock, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect 1");
    let _ = join(&mut sock, "session 1");
    let _ = sample(&mut sock, 0.5);
    send(&mut sock, &ClientMsg::SetBlock { pos: edit_a, block: 13 }); // brick
    send(&mut sock, &ClientMsg::SetBlock { pos: edit_b, block: 6 }); // log
    send(&mut sock, &ClientMsg::GetBlock { pos: edit_a });
    send(&mut sock, &ClientMsg::GetBlock { pos: edit_b });
    let s = sample(&mut sock, 2.0);
    assert_eq!(
        s.block_ats.iter().find(|(q, _)| *q == edit_a).map(|(_, b)| *b),
        Some(13),
        "session 1 must read back its own edit_a (block_ats: {:?})",
        s.block_ats
    );
    assert_eq!(
        s.block_ats.iter().find(|(q, _)| *q == edit_b).map(|(_, b)| *b),
        Some(6),
        "session 1 must read back its own edit_b (block_ats: {:?})",
        s.block_ats
    );
    drop(sock);
    // Wait for the session cleanup (it writes the rejoin record into the
    // registry that the final save snapshots), then a clean stop: the tick
    // loop takes a final save before returning. (The periodic save may not
    // have fired in this short session — the final save is what makes
    // restarts lossless.)
    wait_for_event(&ep.addr, "left (").await;
    ep.shutdown.stop().await;
    assert!(save_path.exists(), "clean stop must write the save file");
    let (saved_seed, saved_edits, saved_players) =
        save::decode(&std::fs::read(&save_path).expect("read save")).expect("decode save");
    assert_eq!(saved_seed, SEED, "save must carry the world's seed");
    assert!(
        saved_players.iter().any(|(_, r)| r.name == "Player"),
        "the v2 save must carry the disconnected player's identity (got {:?})",
        saved_players
    );
    assert!(
        saved_edits.iter().any(|e| e.pos == edit_a && e.block.as_u8() == 13)
            && saved_edits.iter().any(|e| e.pos == edit_b && e.block.as_u8() == 6),
        "save must contain both edits (got {:?})",
        saved_edits
    );

    // Session 2: a FRESH server on the same data dir. It replays the save
    // into fresh terrain; the edits must read back as saved, and untouched
    // terrain must still be pure (a control read at a non-edited spot).
    let control = BlockPos::new(22, 40, 20);
    let ep2 = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect("serve session 2");
    let (mut sock2, _) = connect(&format!("ws://{}/ws", ep2.addr)).expect("connect 2");
    let _ = join(&mut sock2, "session 2");
    let _ = sample(&mut sock2, 0.5);
    send(&mut sock2, &ClientMsg::GetBlock { pos: edit_a });
    send(&mut sock2, &ClientMsg::GetBlock { pos: edit_b });
    send(&mut sock2, &ClientMsg::GetBlock { pos: control });
    let s2 = sample(&mut sock2, 2.0);
    assert_eq!(
        s2.block_ats.iter().find(|(q, _)| *q == edit_a).map(|(_, b)| *b),
        Some(13),
        "edit_a must survive the restart (block_ats: {:?})",
        s2.block_ats
    );
    assert_eq!(
        s2.block_ats.iter().find(|(q, _)| *q == edit_b).map(|(_, b)| *b),
        Some(6),
        "edit_b must survive the restart (block_ats: {:?})",
        s2.block_ats
    );
    // The control position was never edited: it must still be pure terrain
    // — computed here from the seed directly (the world crate is pure).
    let expected_control = qwencraft_server::World::new(SEED).block_at(control).as_u8();
    assert_eq!(
        s2.block_ats.iter().find(|(q, _)| *q == control).map(|(_, b)| *b),
        Some(expected_control),
        "untouched terrain must be intact after the restart (block_ats: {:?})",
        s2.block_ats
    );
    drop(sock2);
    ep2.shutdown.stop().await;
}

/// A save is bound to the seed that generated its terrain. Starting a server
/// with a DIFFERENT seed against an existing save must fail fast (not
/// silently replay edits onto the wrong world).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_seed_mismatch_fails_fast() {
    use qwencraft_server::save;

    let data_dir = test_data_dir("seedmismatch");
    let save_path = data_dir.join(save::SAVE_FILE_NAME);
    // Write a save bound to SEED directly (no session needed).
    std::fs::write(
        &save_path,
        save::encode(
            SEED,
            &[qwencraft_server::Edit {
                pos: BlockPos::new(5, 30, 5),
                block: qwencraft_world::Block::Stone,
            }],
            &[],
        ),
    )
    .expect("write save");

    // Same seed: loads fine.
    let ok = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect("matching seed must load the save");
    ok.shutdown.stop().await;

    // Different seed: must refuse to start.
    let err = serve(ServerOptions {
        seed: SEED + 1,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect_err("a seed mismatch must fail fast");
    assert!(
        err.contains("seed"),
        "the error should mention the seed mismatch (got: {err})"
    );
}

/// A full rejoin cycle within one server run: the player claims a fresh
/// identity, picks a name/colour, moves far from spawn, and leaves. A new
/// connection presenting the stored token must be restored to the same
/// spot with the same name/colour — and get the SAME token back. Unknown
/// and all-zero tokens must get fresh identities at the fresh spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejoin_reclaims_identity() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: test_data_dir("rejoin"),
        ..Default::default()
    })
    .await
    .expect("serve");

    // Connection 1: fresh identity, profile, move, leave.
    let (mut a, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect A");
    let (_pid_a, token_a) = join(&mut a, "A");
    send(&mut a, &ClientMsg::Profile { name: "Alice".into(), color: [10, 200, 255] });
    let s0 = sample(&mut a, 1.0);
    let p0 = s0.player.expect("player state");
    let dest = Vec3::new(p0.pos.x + 40.5, p0.pos.y, p0.pos.z - 25.5);
    send(&mut a, &ClientMsg::Teleport { pos: dest });
    let s1 = sample(&mut a, 1.5);
    let last = s1.player.expect("player state after teleport");
    assert!((last.pos.x - dest.x).abs() < 3.0, "the teleport must land (got {:?})", last.pos);
    assert!(
        s1.player_names.contains(&"Alice".to_string()),
        "the profile must be live before the leave (names: {:?})",
        s1.player_names
    );
    drop(a);
    wait_for_event(&ep.addr, "left (").await;

    // Connection 2: present the token → the previous instance comes back.
    let (mut b, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect B");
    send(&mut b, &ClientMsg::Rejoin { token: token_a });
    let (_pid_b, token_b) = expect_hello(&mut b, "B");
    assert_eq!(token_b, token_a, "a recognised rejoin must keep the token");
    let s2 = sample(&mut b, 1.5);
    let p2 = s2.player.expect("player state after rejoin");
    assert!(
        dist(p2.pos, last.pos) < 1.5,
        "rejoin must restore the pre-leave position (was {:?}, now {:?})",
        last.pos,
        p2.pos
    );
    assert!(
        s2.player_names.contains(&"Alice".to_string()),
        "rejoin must keep the name (names: {:?})",
        s2.player_names
    );
    assert_eq!(p2.color, [10, 200, 255], "rejoin must keep the sphere colour");

    // Connection 3: a token this world never minted → fresh identity, fresh
    // spawn (far from Alice's spot), a NEW token.
    drop(b);
    wait_for_event(&ep.addr, "left (").await;
    let (mut c, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect C");
    send(&mut c, &ClientMsg::Rejoin { token: [0x42; 16] });
    let (_pid_c, token_c) = expect_hello(&mut c, "C");
    assert_ne!(token_c, [0x42; 16], "an unknown token must not be honoured");
    assert_ne!(token_c, [0u8; 16], "a fresh identity must get a token");
    let s3 = sample(&mut c, 1.0);
    let p3 = s3.player.expect("player state");
    assert!(
        dist(p3.pos, last.pos) > 20.0,
        "an unknown token must spawn fresh, not at the recorded spot (got {:?})",
        p3.pos
    );
    drop(c);
    ep.shutdown.stop().await;
}

/// A second connection presenting a token that is ALREADY connected (a
/// second tab with the same browser profile) must not hijack the live
/// player: it gets a fresh identity, the original connection is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_live_token_gets_fresh_identity() {
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: test_data_dir("duplive"),
        ..Default::default()
    })
    .await
    .expect("serve");

    let (mut a, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect A");
    let (_pid_a, token_a) = join(&mut a, "A");

    // B claims A's LIVE token: rejected → fresh identity.
    let (mut b, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect B");
    send(&mut b, &ClientMsg::Rejoin { token: token_a });
    let (_pid_b, token_b) = expect_hello(&mut b, "B");
    assert_ne!(token_b, token_a, "a live identity must not be hijacked");
    assert_ne!(token_b, [0u8; 16], "the fresh identity must get its own token");

    // A is still fully alive and streaming its own state.
    let sa = sample(&mut a, 0.5);
    assert!(sa.player.is_some(), "A must be unaffected by B's rejected claim");
    drop(b);
    wait_for_event(&ep.addr, "left (").await;
    drop(a);
    wait_for_event(&ep.addr, "left (0 online)").await;
    ep.shutdown.stop().await;
}

/// Rejoin across a server RESTART: the identity is persisted in the
/// seed-bound world save, so a fresh server on the same data dir restores
/// the pre-restart position and name (the token travels only through the
/// client — here, a variable).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejoin_survives_restart() {
    let data_dir = test_data_dir("rejoinrestart");

    // Session 1: identity, profile, move, clean stop.
    let ep = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect("serve 1");
    let (mut sock, _) = connect(&format!("ws://{}/ws", ep.addr)).expect("connect 1");
    let (_pid, token) = join(&mut sock, "session 1");
    send(&mut sock, &ClientMsg::Profile { name: "Alice".into(), color: [10, 200, 255] });
    let s0 = sample(&mut sock, 1.0);
    let p0 = s0.player.expect("player state");
    let dest = Vec3::new(p0.pos.x + 40.5, p0.pos.y, p0.pos.z - 25.5);
    send(&mut sock, &ClientMsg::Teleport { pos: dest });
    let s1 = sample(&mut sock, 1.5);
    let last = s1.player.expect("player state after teleport");
    drop(sock);
    wait_for_event(&ep.addr, "left (").await;
    ep.shutdown.stop().await;

    // Session 2: fresh server, same data dir. Present the token.
    let ep2 = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect("serve 2");
    let (mut sock2, _) = connect(&format!("ws://{}/ws", ep2.addr)).expect("connect 2");
    send(&mut sock2, &ClientMsg::Rejoin { token });
    let (_pid2, token2) = expect_hello(&mut sock2, "session 2");
    assert_eq!(token2, token, "the restart must keep the identity's token");
    let s2 = sample(&mut sock2, 1.5);
    let p2 = s2.player.expect("player state");
    assert!(
        dist(p2.pos, last.pos) < 1.5,
        "rejoin after restart must restore the pre-restart position (was {:?}, now {:?})",
        last.pos,
        p2.pos
    );
    assert!(
        s2.player_names.contains(&"Alice".to_string()),
        "rejoin after restart must keep the name (names: {:?})",
        s2.player_names
    );
    assert_eq!(p2.color, [10, 200, 255], "rejoin after restart must keep the colour");

    // And the stop-while-connected case: restart with the player STILL
    // connected (no leave event ever ran) — the clean stop must fold their
    // state into the save, and the token must still work.
    let p_before = p2.pos;
    ep2.shutdown.stop().await;
    drop(sock2);
    let ep3 = serve(ServerOptions {
        seed: SEED,
        port: 0,
        data_dir: data_dir.clone(),
        ..Default::default()
    })
    .await
    .expect("serve 3");
    let (mut sock3, _) = connect(&format!("ws://{}/ws", ep3.addr)).expect("connect 3");
    send(&mut sock3, &ClientMsg::Rejoin { token });
    let (_pid3, token3) = expect_hello(&mut sock3, "session 3");
    assert_eq!(token3, token, "the stop-while-connected restart must keep the token");
    let s3 = sample(&mut sock3, 1.5);
    let p3 = s3.player.expect("player state");
    assert!(
        dist(p3.pos, p_before) < 1.5,
        "rejoin after a stop-while-connected must restore the position (was {:?}, now {:?})",
        p_before,
        p3.pos
    );
    assert!(
        s3.player_names.contains(&"Alice".to_string()),
        "the name must survive the stop-while-connected restart (names: {:?})",
        s3.player_names
    );
    drop(sock3);
    ep3.shutdown.stop().await;
}
