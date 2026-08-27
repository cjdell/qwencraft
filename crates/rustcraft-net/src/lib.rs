//! Headless RustCraft server.
//!
//! Serves the authoritative [`rustcraft_server::Server`] over WebSocket
//! (`ws://`, or `wss://` with `--cert/--key`). The wire protocol is
//! [`rustcraft_server::protocol`]: the client sends input/actions, the server
//! sends player/agent state, chunk regions and stats at a fixed 60 Hz.
//!
//! **One shared world per server process.** Every connection gets its own
//! player agent in the *same* world, so clients see each other and every
//! world edit: a single 60 Hz tick loop drives the shared [`Server`], and each
//! connection owns a [`Streamer`] that streams the world around its own player
//! and resends edited chunks. Disconnecting removes that player (the world and
//! the other players remain).
//!
//! Host-only (tokio/mio don't support wasm): the whole crate compiles away
//! for wasm so the shared workspace's wasm build stays green.

#![cfg(not(target_arch = "wasm32"))]

pub mod http;
pub mod map;

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rustcraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use rustcraft_server::{
    Action, Input, KeySet, Server, Streamer, WorldUpdate, TICK_HZ,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Server options (see [`serve`]).
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// World seed.
    pub seed: u64,
    /// Interface to bind.
    pub bind: IpAddr,
    /// WebSocket port (0 = let the OS pick; see [`serve`]).
    pub port: u16,
    /// TLS certificate (PEM). With [`Self::key`], the server speaks wss://.
    pub cert: Option<PathBuf>,
    /// TLS private key (PEM, RSA or PKCS#8).
    pub key: Option<PathBuf>,
    /// Dashboard HTTP port (0 = let the OS pick). `None` disables the
    /// dashboard entirely.
    pub http_port: Option<u16>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            seed: 1337,
            bind: "0.0.0.0".parse().unwrap(),
            port: 9000,
            cert: None,
            key: None,
            http_port: Some(9001),
        }
    }
}

/// The actual bound addresses after [`serve`].
pub struct ServerEndpoints {
    /// The WebSocket game server.
    pub ws: SocketAddr,
    /// The dashboard HTTP server (when enabled).
    pub http: Option<SocketAddr>,
}

/// Bounded, thread-safe event log for the dashboard (join/leave, world
/// edits, NPC load, …). Its own mutex: the tick loop pushes server events
/// from inside the world lock, so the log must never be guarded by the
/// world mutex itself (that would deadlock the event sink).
pub struct EventLog {
    inner: Mutex<VecDeque<(f64, String)>>,
    cap: usize,
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            cap: 256,
        }
    }

    /// Record one event (`t` = seconds since server start).
    pub fn push(&self, t: f64, msg: String) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.cap {
            q.pop_front();
        }
        q.push_back((t, msg));
    }

    /// A copy of the log, oldest first.
    pub fn snapshot(&self) -> Vec<(f64, String)> {
        self.inner.lock().unwrap().iter().cloned().collect()
    }
}

/// The shared world: the authoritative [`Server`] plus the live connections.
/// Each connection owns one player agent (keyed by player id) and a
/// [`Streamer`] for that player's view. Public (the dashboard HTTP front end
/// takes a handle to it); the fields stay private.
pub struct WorldState {
    server: Server,
    players: HashMap<u32, Conn>,
    /// Dashboard event log (separate lock — see [`EventLog`]).
    events: Arc<EventLog>,
    /// Process start (dashboard uptime).
    started: Instant,
}

/// One connected client.
struct Conn {
    /// Outbound server messages (the session's writer task reads these).
    tx: mpsc::UnboundedSender<ServerMsg>,
    /// This viewer's chunk-streaming state (sent set + outbound queue).
    streamer: Streamer,
}

/// Load the TLS acceptor, bind the listeners, and spawn the shared-world
/// tick loop, the WebSocket accept loop, and (when enabled) the dashboard
/// HTTP server.
///
/// Returns the actual bound addresses (pass `port: 0` / `http_port: Some(0)`
/// to let the OS pick one — used by the end-to-end tests). The loops keep
/// running for the life of the current tokio runtime.
pub async fn serve(opts: ServerOptions) -> Result<ServerEndpoints, String> {
    let tls = load_tls(&opts)?;
    let listener = TcpListener::bind((opts.bind, opts.port))
        .await
        .map_err(|e| format!("bind {}:{}: {e}", opts.bind, opts.port))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    eprintln!(
        "[rustcraft-net] listening on {scheme}://{addr} (seed {}, shared world — one player per connection)",
        opts.seed
    );

    let started = Instant::now();
    let events = Arc::new(EventLog::new());
    let world = Arc::new(Mutex::new(WorldState {
        server: Server::new_world(opts.seed),
        players: HashMap::new(),
        events: events.clone(),
        started,
    }));
    // Dashboard map state (its own lock; computed off the tick path).
    let map = Arc::new(Mutex::new(map::MapState::new(opts.seed)));

    // World edits / fly / NPC events from the server feed the dashboard log.
    // The sink captures only the (separately-locked) event log + start time:
    // it is invoked while the tick loop holds the world lock, so it must not
    // take that lock back.
    {
        let events_for_sink = events.clone();
        let started_for_sink = started;
        world.lock().unwrap().server.set_event_sink(Some(Arc::new(
            move |msg: &str| {
                events_for_sink.push(started_for_sink.elapsed().as_secs_f64(), msg.to_string());
            },
        )));
    }

    events.push(
        0.0,
        format!("server started (seed {}, {scheme}://{addr})", opts.seed),
    );

    // The single tick loop for the shared world.
    {
        let world = world.clone();
        let map = map.clone();
        tokio::spawn(tick_loop(world, map));
    }
    let seed = opts.seed;
    // The accept loop below moves its capture, but the dashboard HTTP
    // server still needs a handle to the world — hand it its own clone.
    let world_accept = world.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp, peer)) => {
                    let tls = tls.clone();
                    let world = world_accept.clone();
                    tokio::spawn(async move {
                        match handle_conn(tcp, peer, world, seed, tls).await {
                            Ok(()) => {}
                            Err(e) => eprintln!("[rustcraft-net] {peer}: {e}"),
                        }
                    });
                }
                Err(e) => eprintln!("[rustcraft-net] accept failed: {e}"),
            }
        }
    });

    // Dashboard HTTP server (optional).
    let http = match opts.http_port {
        Some(p) => Some(http::run_http(opts.bind, p, world.clone(), map).await?),
        None => None,
    };
    if let Some(h) = &http {
        events.push(
            started.elapsed().as_secs_f64(),
            format!("dashboard enabled (http://{h})"),
        );
    }
    Ok(ServerEndpoints { ws: addr, http })
}

/// The single 60 Hz tick loop for the shared world. Ticks the [`Server`],
/// then for each connected player streams the world around them and sends
/// that player's state, all agents, and stats. New world edits are also
/// forwarded to the dashboard map state (last-wins overlay).
async fn tick_loop(world: Arc<Mutex<WorldState>>, map: Arc<Mutex<map::MapState>>) {
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / TICK_HZ as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = Instant::now();
    // Sentinel so the initial NPC load is echoed once to every connection.
    let mut last_npc_load: (u32, f32) = (u32::MAX, f32::NAN);
    // How much of the world's (append-only) edit history has been synced to
    // the dashboard map already.
    let mut last_edit_seq = 0usize;

    loop {
        tick.tick().await;
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();

        let mut w = world.lock().unwrap();
        // Destructure into disjoint field references so the per-connection
        // loop can touch `players` and `server` independently (the MutexGuard
        // itself doesn't field-split through its deref).
        let WorldState { server, players, .. } = &mut *w;
        server.tick(dt);
        let dirty = server.drain_dirty();
        // Dashboard map: sync edits added by this tick (usually zero).
        {
            let edits = server.world().edits();
            if edits.len() > last_edit_seq {
                let mut m = map.lock().unwrap();
                m.sync_edits(&edits[last_edit_seq..]);
                last_edit_seq = edits.len();
            }
        }
        let agents = server.agents();
        let base_stats = server.stats(0);
        let npc_load = server.npc_load_config();

        for (id, conn) in players.iter_mut() {
            // Edits are visible to every viewer that holds the chunk.
            conn.streamer.apply_edits(server.world(), &dirty);
            let vp = server.agent_state(*id).pos;
            conn.streamer.tick(server.world_mut(), vp);
            for u in conn.streamer.take() {
                let WorldUpdate::Chunk { pos, data } = u;
                let _ = conn.tx.send(ServerMsg::Chunk { pos, data });
            }
            let _ = conn
                .tx
                .send(ServerMsg::PlayerState(server.agent_state(*id)));
            let _ = conn.tx.send(ServerMsg::Agents(agents.clone()));
            let mut stats = base_stats;
            stats.chunks_sent = conn.streamer.sent_count();
            let _ = conn.tx.send(ServerMsg::Stats(stats));
        }

        if npc_load != last_npc_load {
            last_npc_load = npc_load;
            for conn in players.values() {
                let _ = conn.tx.send(ServerMsg::NpcLoad {
                    count: npc_load.0,
                    spacing: npc_load.1,
                });
            }
        }
    }
}

/// WebSocket handshake (optionally over TLS), then one player in the shared
/// world.
async fn handle_conn(
    tcp: TcpStream,
    peer: SocketAddr,
    world: Arc<Mutex<WorldState>>,
    seed: u64,
    tls: Option<Arc<tokio_rustls::server::TlsAcceptor>>,
) -> Result<(), String> {
    // `session` is generic over the stream type, so each (plain / TLS) arm
    // calls it directly rather than unifying the two stream types here.
    match tls {
        Some(acceptor) => {
            let stream = acceptor
                .accept(tcp)
                .await
                .map_err(|e| format!("TLS handshake: {e}"))?;
            let ws = tokio_tungstenite::accept_async(stream)
                .await
                .map_err(|e| format!("WebSocket handshake: {e}"))?;
            session(ws, peer, world, seed).await
        }
        None => {
            let ws = tokio_tungstenite::accept_async(tcp)
                .await
                .map_err(|e| format!("WebSocket handshake: {e}"))?;
            session(ws, peer, world, seed).await
        }
    }
}

/// Apply one decoded client message to the shared world (input/actions are
/// stored per-player; the tick loop applies them on the next step).
fn apply_inbound(world: &Mutex<WorldState>, player_id: u32, m: ClientMsg) {
    let mut w = world.lock().unwrap();
    let WorldState { server, players, .. } = &mut *w;
    match m {
        ClientMsg::Input { keys, dx, dy } => {
            let mut input = Input::default();
            input.keys = KeySet::from_bits(keys);
            input.mouse_dx = dx;
            input.mouse_dy = dy;
            server.set_agent_input(player_id, input);
        }
        ClientMsg::Action(a) => {
            server.push_agent_action(player_id, a);
        }
        ClientMsg::Evicted(evicted) => {
            // Pool eviction: forget the chunks for this viewer; its stream
            // re-sends the ones that are visible again (next tick).
            if let Some(conn) = players.get_mut(&player_id) {
                conn.streamer.note_evicted(&evicted);
            }
        }
        ClientMsg::SetNpcLoad { count, spacing } => {
            server.set_npc_load(count, spacing);
            server.push_agent_action(player_id, Action::NpcLoad);
        }
    }
}

/// Serve one connection: register a player in the shared world, forward
/// inbound client messages to it, and stream outbound server messages to the
/// socket. The world ticks on the shared [`tick_loop`]; this task only moves
/// data between the socket and the world.
async fn session<S>(
    ws: WebSocketStream<S>,
    peer: SocketAddr,
    world: Arc<Mutex<WorldState>>,
    seed: u64,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut wr, mut rd): (
        SplitSink<WebSocketStream<S>, Message>,
        SplitStream<WebSocketStream<S>>,
    ) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();

    // Register this connection as a new player in the shared world.
    let player_id = {
        let mut w = world.lock().unwrap();
        let id = w.server.add_player();
        w.players
            .insert(id, Conn { tx: tx.clone(), streamer: Streamer::new() });
        w.events.push(
            w.started.elapsed().as_secs_f64(),
            format!("player {id} joined (from {peer}, {} online)", w.players.len()),
        );
        id
    };
    eprintln!(
        "[rustcraft-net] {peer}: player {} joined (shared world seed {seed}, {} online)",
        player_id,
        world.lock().unwrap().players.len()
    );

    // Hello first: the client waits for it before sending input.
    let _ = tx.send(ServerMsg::Hello { version: PROTOCOL_VERSION, seed });

    // Reader: decode client messages and apply them to the shared world.
    let reader_world = world.clone();
    let reader = tokio::spawn(async move {
        while let Some(frame) = rd.next().await {
            match frame {
                // tungstenite answers pings automatically (protocol layer),
                // so nothing to do for control frames.
                Ok(Message::Binary(data)) => {
                    for m in ClientMsg::decode_stream(&data).0 {
                        apply_inbound(&reader_world, player_id, m);
                    }
                }
                Ok(Message::Close(_)) | Ok(Message::Text(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    // Writer: forward server messages to the socket until it breaks.
    let result = 'outer: loop {
        match rx.recv().await {
            Some(msg) => {
                if wr
                    .send(Message::Binary(msg.encode().into()))
                    .await
                    .map_err(|e| format!("send: {e}"))
                    .is_err()
                {
                    break 'outer Err(());
                }
            }
            None => break 'outer Ok(()),
        }
    };

    reader.abort();
    // Deregister: remove this player from the shared world (the world and the
    // other players remain).
    {
        let mut w = world.lock().unwrap();
        w.players.remove(&player_id);
        w.server.remove_player(player_id);
        w.events.push(
            w.started.elapsed().as_secs_f64(),
            format!("player {player_id} left ({} online)", w.players.len()),
        );
    }
    eprintln!(
        "[rustcraft-net] {peer}: player {} left ({} online)",
        player_id,
        world.lock().unwrap().players.len()
    );
    result.map_err(|()| "socket closed".to_string())
}

/// Build the TLS acceptor from the PEM cert/key files (None when not set).
fn load_tls(opts: &ServerOptions) -> Result<Option<Arc<tokio_rustls::server::TlsAcceptor>>, String> {
    match (&opts.cert, &opts.key) {
        (Some(cert_path), Some(key_path)) => {
            let mut cert_file = BufReader::new(
                File::open(cert_path).map_err(|e| format!("open {}: {e}", cert_path.display()))?,
            );
            let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                rustls_pemfile::certs(&mut cert_file)
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("parse {}: {e}", cert_path.display()))?;
            if certs.is_empty() {
                return Err(format!("no certificates in {}", cert_path.display()));
            }
            let mut key_file = BufReader::new(
                File::open(key_path).map_err(|e| format!("open {}: {e}", key_path.display()))?,
            );
            let key = rustls_pemfile::private_key(&mut key_file)
                .map_err(|e| format!("parse {}: {e}", key_path.display()))?
                .ok_or_else(|| format!("no private key in {}", key_path.display()))?;
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|e| format!("TLS config: {e}"))?;
            Ok(Some(Arc::new(tokio_rustls::server::TlsAcceptor::from(Arc::new(config)))))
        }
        (None, None) => Ok(None),
        _ => Err("--cert and --key must be given together".into()),
    }
}
