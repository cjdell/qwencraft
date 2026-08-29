//! Headless Qwencraft server.
//!
//! One TCP port hosts everything (one authority):
//! - **`/ws`** — the authoritative game world over WebSocket (`ws://`, or
//!   `wss://` with `--cert/--key`), one shared world for all connections;
//! - **`/dashboard`** (+ `/api/status`, `/api/map`) — the operator
//!   dashboard;
//! - **`/`** — the embedded game client build (`web/dist`, when present at
//!   build time), so the whole game can live on this one port.
//!
//! The wire protocol is [`qwencraft_server::protocol`]: the client sends
//! input/actions, the server sends player/agent state, chunk regions and
//! stats at a fixed 60 Hz.
//!
//! **One shared world per server process.** Every connection gets its own
//! player agent in the *same* world, so clients see each other (sphere +
//! name tag) and every world edit: a single 60 Hz tick loop drives the
//! shared [`Server`], and each connection owns a [`Streamer`] that streams
//! the world around its own player and resends edited chunks.
//! Disconnecting removes that player (the world and the other players
//! remain).
//!
//! Host-only (tokio/mio don't support wasm): the whole crate compiles away
//! for wasm so the shared workspace's wasm build stays green.

#![cfg(not(target_arch = "wasm32"))]

pub mod http;
pub mod map;

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::BufReader as StdBufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use qwencraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use qwencraft_server::{
    Action, Input, KeySet, Server, Streamer, WorldUpdate, TICK_HZ,
};
use qwencraft_world::Block;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// The WebSocket endpoint path on the server's single port.
pub const WS_PATH: &str = "/ws";

/// Server options (see [`serve`]).
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// World seed.
    pub seed: u64,
    /// Interface to bind.
    pub bind: IpAddr,
    /// TCP port: WebSocket at `/ws`, dashboard at `/dashboard`, game
    /// client at `/` (0 = let the OS pick; see [`serve`]).
    pub port: u16,
    /// TLS certificate (PEM). With [`Self::key`], the whole port speaks
    /// `wss://` / `https://`.
    pub cert: Option<PathBuf>,
    /// TLS private key (PEM, RSA or PKCS#8).
    pub key: Option<PathBuf>,
    /// Per-second per-player streaming telemetry to stderr (`--debug`).
    pub debug: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            seed: 1337,
            bind: "0.0.0.0".parse().unwrap(),
            port: 9000,
            cert: None,
            key: None,
            debug: false,
        }
    }
}

/// The actual bound address after [`serve`].
pub struct ServerEndpoints {
    /// The single TCP port: `ws://addr/ws`, `http://addr/dashboard/`,
    /// `http://addr/` (game client).
    pub addr: SocketAddr,
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

/// Load the TLS acceptor, bind the single listener, and spawn the shared
/// world tick loop plus the accept/dispatch loop (WebSocket at `/ws`, HTTP
/// everywhere else).
///
/// Returns the actual bound address (pass `port: 0` to let the OS pick one
/// — used by the end-to-end tests). The loops keep running for the life of
/// the current tokio runtime.
pub async fn serve(opts: ServerOptions) -> Result<ServerEndpoints, String> {
    let tls = load_tls(&opts)?;
    let listener = TcpListener::bind((opts.bind, opts.port))
        .await
        .map_err(|e| format!("bind {}:{}: {e}", opts.bind, opts.port))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    let http_scheme = if tls.is_some() { "https" } else { "http" };
    eprintln!(
        "[qwencraft-net] listening on {scheme}://{addr}{WS_PATH} + {http_scheme}://{addr}/dashboard (seed {}, shared world — one player per connection)",
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
    let debug = opts.debug;
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
        format!(
            "server started (seed {}, {scheme}://{addr}{WS_PATH})",
            opts.seed
        ),
    );

    // The single tick loop for the shared world.
    {
        let world = world.clone();
        let map = map.clone();
        tokio::spawn(tick_loop(world, map, debug));
    }
    let seed = opts.seed;
    let world_accept = world.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp, peer)) => {
                    let tls = tls.clone();
                    let world = world_accept.clone();
                    let map = map.clone();
                    tokio::spawn(async move {
                        match dispatch_conn(tcp, peer, world, map, seed, tls).await {
                            Ok(()) => {}
                            Err(e) => eprintln!("[qwencraft-net] {peer}: {e}"),
                        }
                    });
                }
                Err(e) => eprintln!("[qwencraft-net] accept failed: {e}"),
            }
        }
    });

    Ok(ServerEndpoints { addr })
}

/// Route one accepted connection: read the first bytes, then
/// - `/ws` → WebSocket handshake + game session,
/// - anything else → the dashboard / game-client HTTP front end.
///
/// With TLS configured, every connection starts with a TLS handshake (the
/// port speaks wss/https end to end); without it, a TLS client is detected
/// by its record header and rejected with a hint.
async fn dispatch_conn(
    mut tcp: TcpStream,
    peer: SocketAddr,
    world: Arc<Mutex<WorldState>>,
    map: Arc<Mutex<map::MapState>>,
    seed: u64,
    tls: Option<Arc<tokio_rustls::server::TlsAcceptor>>,
) -> Result<(), String> {
    match &tls {
        Some(acceptor) => {
            let mut stream = acceptor
                .accept(tcp)
                .await
                .map_err(|e| format!("TLS handshake: {e}"))?;
            let mut head = Vec::new();
            read_head(&mut stream, &mut head).await?;
            route_request(stream, head, peer, world, map, seed).await
        }
        None => {
            // Read the first bytes and check for a TLS record header before
            // committing to plaintext (a wss:// client against a ws://
            // server gets a hint instead of a hung connection).
            let mut lead = [0u8; 2];
            let n = tcp.read(&mut lead).await.map_err(|e| format!("read: {e}"))?;
            if n >= 2 && lead[0] == 0x16 && lead[1] == 0x03 {
                return Err(
                    "TLS client on a non-TLS port — restart with --cert/--key for wss://"
                        .to_string(),
                );
            }
            if n == 0 {
                return Err("connection closed before any request".to_string());
            }
            let mut head = lead[..n].to_vec();
            read_head(&mut tcp, &mut head).await?;
            route_request(tcp, head, peer, world, map, seed).await
        }
    }
}

/// Read an HTTP/WS request head (up to the header terminator or [`HEAD_MAX`]
/// bytes) from `stream`, appending to `head` (which may already hold the
/// first bytes). A 10 s deadline keeps a slow/idle connection from pinning
/// a task.
///
/// The bytes are consumed from the stream on purpose: [`route_request`]
/// re-prepends them (via `tokio::io::chain`) so the WebSocket/HTTP parser
/// sees the exact same request it would have seen on the bare stream.
const HEAD_MAX: usize = 8192;

async fn read_head<S>(stream: &mut S, head: &mut Vec<u8>) -> Result<(), String>
where
    S: AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut tmp = [0u8; 2048];
        let n = tokio::time::timeout_at(deadline, stream.read(&mut tmp))
            .await
            .map_err(|_| "timed out reading request head".to_string())?
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            if head.is_empty() {
                return Err("connection closed before any request".to_string());
            }
            return Ok(());
        }
        head.extend_from_slice(&tmp[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() >= HEAD_MAX {
            return Ok(());
        }
    }
}

/// True when the request head carries an `Upgrade: …websocket…` header
/// (case-insensitive). Only the header section is inspected; the request
/// line comes first.
fn has_websocket_upgrade(head: &[u8]) -> bool {
    let lower = head.to_ascii_lowercase();
    let Some(line_end) = lower.iter().position(|&b| b == b'\n') else {
        return false;
    };
    let headers = &lower[line_end..];
    headers.split(|&b| b == b'\n').any(|line| {
        line.starts_with(b"upgrade:")
            && line.windows(9).any(|w| w == b"websocket")
    })
}

/// The request target (second token of the first line), or None.
fn request_target(head: &[u8]) -> Option<String> {
    let line = head.split(|&b| b == b'\n').next()?;
    let s = std::str::from_utf8(line).ok()?;
    let target = s.split_whitespace().nth(1)?;
    Some(target.trim_end_matches(['\r', '\n']).to_string())
}

/// A bidirectional stream that first replays `head` — the bytes the
/// dispatcher pre-read to sniff the request — and then proxies to `inner`.
/// Without the replay, the WebSocket handshake / HTTP parser would start
/// mid-request and block forever (the client is waiting for the response).
struct Prepended<T> {
    head: std::io::Cursor<Vec<u8>>,
    inner: T,
}

impl<T> Prepended<T> {
    fn new(head: Vec<u8>, inner: T) -> Self {
        Self {
            head: std::io::Cursor::new(head),
            inner,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Prepended<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // The replayed bytes are always ready (no I/O involved).
        let pos = self.head.position() as usize;
        let end = self.head.get_ref().len();
        if pos < end {
            let n = (end - pos).min(buf.remaining());
            buf.put_slice(&self.head.get_ref()[pos..pos + n]);
            self.head.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Prepended<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Split the dispatcher's pre-read into a game session or an HTTP request.
///
/// The pre-read head bytes are re-prepended to the stream (see
/// [`Prepended`]) so the WebSocket handshake / HTTP parser sees the
/// original request intact.
async fn route_request<S>(
    stream: S,
    head: Vec<u8>,
    peer: SocketAddr,
    world: Arc<Mutex<WorldState>>,
    map: Arc<Mutex<map::MapState>>,
    seed: u64,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let target = request_target(&head);
    let wants_ws = target.as_deref().is_some_and(|t| {
        t.strip_prefix(WS_PATH)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('?'))
    });
    // A real WebSocket upgrade carries the `Upgrade: websocket` header;
    // anything else on /ws (e.g. a browser tab) goes to the HTTP front end,
    // which answers a friendly 426 instead of a hung connection.
    let is_ws = wants_ws && has_websocket_upgrade(&head);
    let stream = Prepended::new(head, stream);
    if is_ws {
        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .map_err(|e| format!("WebSocket handshake: {e}"))?;
        session(ws, peer, world, seed).await
    } else {
        http::handle_http(stream, world, map).await
    }
}

/// The single 60 Hz tick loop for the shared world. Ticks the [`Server`],
/// then for each connected player streams the world around them and sends
/// that player's state, all agents, and stats. New world edits are also
/// forwarded to the dashboard map state (last-wins overlay).
async fn tick_loop(world: Arc<Mutex<WorldState>>, map: Arc<Mutex<map::MapState>>, debug: bool) {
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / TICK_HZ as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = Instant::now();
    let t0 = last;
    let mut tick_count = 0u64;
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

        tick_count += 1;
        if debug && tick_count % 60 == 0 { // one line per second at 60 Hz
            // Per-second per-player streaming telemetry: how many distinct
            // regions this viewer has been sent, how many are queued for
            // delivery right now, and where the player is.
            let t = t0.elapsed().as_secs_f64();
            for (id, conn) in players.iter() {
                let p = server.agent_state(*id).pos;
                eprintln!(
                    "[dbg] t={t:.1}s player {id}: sent={} queue={} pos=({:.0},{:.0},{:.0})",
                    conn.streamer.sent_count(),
                    conn.streamer.queued_count(),
                    p.x, p.y, p.z
                );
            }
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
        ClientMsg::Profile { name, color } => {
            // Identity for the shared world: broadcast via the agent list
            // so every other client sees this player's name + sphere colour.
            server.set_profile(player_id, name, color);
        }
        ClientMsg::Resync(have) => {
            // Transit-loss reconciliation: the client reports the chunks it
            // holds; re-send everything in its view radius it doesn't (the
            // streamer's sent set can't detect chunks lost before the
            // client ever saw them — see ClientMsg::Resync).
            if let Some(conn) = players.get_mut(&player_id) {
                let vp = server.agent_state(player_id).pos;
                let n = conn.streamer.resync(server.world(), vp, &have);
                eprintln!(
                    "[qwencraft-net] player {player_id}: resync — {n} chunk regions re-sent (transit loss detected)"
                );
            }
        }
        ClientMsg::GetBlock { pos } => {
            // Console API: answer from the authoritative world (the client
            // never reads its own streamed copy), to this connection only.
            let block = server.block_at(pos);
            if let Some(conn) = players.get(&player_id) {
                let _ = conn.tx.send(ServerMsg::BlockAt { pos, block: block.as_u8() });
            }
        }
        ClientMsg::SetBlock { pos, block } => {
            // Console API: the same world-write path as a player edit
            // (dirty chunks re-send to every viewer holding them on the
            // next tick); the server validates and reports rejections.
            if let Err(e) = server.console_edit_block(player_id, pos, Block::from_u8(block)) {
                eprintln!("[qwencraft-net] player {player_id}: console edit rejected: {e}");
            }
        }
        ClientMsg::Teleport { pos } => {
            if let Err(e) = server.console_teleport(player_id, pos) {
                eprintln!("[qwencraft-net] player {player_id}: teleport rejected: {e}");
            }
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
        "[qwencraft-net] {peer}: player {} joined (shared world seed {seed}, {} online)",
        player_id,
        world.lock().unwrap().players.len()
    );

    // Hello first (carries this connection's own player id so the client
    // can render the *other* players): the client waits for it before
    // sending input.
    let _ = tx.send(ServerMsg::Hello {
        version: PROTOCOL_VERSION,
        seed,
        player_id,
    });

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
        "[qwencraft-net] {peer}: player {} left ({} online)",
        player_id,
        world.lock().unwrap().players.len()
    );
    result.map_err(|()| "socket closed".to_string())
}

/// Build the TLS acceptor from the PEM cert/key files (None when not set).
fn load_tls(opts: &ServerOptions) -> Result<Option<Arc<tokio_rustls::server::TlsAcceptor>>, String> {
    match (&opts.cert, &opts.key) {
        (Some(cert_path), Some(key_path)) => {
            let mut cert_file = StdBufReader::new(
                File::open(cert_path).map_err(|e| format!("open {}: {e}", cert_path.display()))?,
            );
            let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                rustls_pemfile::certs(&mut cert_file)
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("parse {}: {e}", cert_path.display()))?;
            if certs.is_empty() {
                return Err(format!("no certificates in {}", cert_path.display()));
            }
            let mut key_file = StdBufReader::new(
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
