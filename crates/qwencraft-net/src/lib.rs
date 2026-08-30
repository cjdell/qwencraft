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
//! remain) and records its last state under its rejoin token.
//!
//! **Rejoin (protocol v8).** Every identity gets a 16-byte token (in
//! `Hello`); the client persists it (localStorage) and presents it via the
//! first-frame `ClientMsg::Rejoin` on its next visit. The token is looked
//! up in the [`PlayerRegistry`] — persisted in the seed-bound world save,
//! so a token only works against the world it was minted for — and the
//! player is restored to their last position/view/name/colour. No auth:
//! the token is a capability, not a credential.
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use qwencraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use qwencraft_server::save::PlayerRecord;
use qwencraft_server::{
    save, Action, Edit, Input, KeySet, Server, Streamer, WorldUpdate, TICK_HZ,
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
    /// World seed. If a save file exists in [`Self::data_dir`], its seed
    /// must match (the save is bound to the seed that generated it).
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
    /// Directory holding the world save file (`world.save`). Created on
    /// first save. The world's block edits are snapshotted here
    /// periodically and on clean shutdown (see [`serve`]).
    pub data_dir: PathBuf,
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
            data_dir: PathBuf::from("data"),
        }
    }
}

/// The actual bound address after [`serve`], plus the shutdown handle.
pub struct ServerEndpoints {
    /// The single TCP port: `ws://addr/ws`, `http://addr/dashboard/`,
    /// `http://addr/` (game client).
    pub addr: SocketAddr,
    /// Signals a clean stop and waits for it (including the final world
    /// save). Dropping it without [`ServerShutdown::stop`] lets the runtime
    /// tear the loops down without saving.
    pub shutdown: ServerShutdown,
}

impl std::fmt::Debug for ServerEndpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerEndpoints")
            .field("addr", &self.addr)
            .finish()
    }
}

/// Clean-stop handle for the shared-world tick loop (returned by [`serve`]).
pub struct ServerShutdown {
    stop: tokio::sync::watch::Sender<bool>,
    tick_task: tokio::task::JoinHandle<()>,
}

impl ServerShutdown {
    /// Signal the tick loop to stop and wait for it to finish — including
    /// the final world save. The rest of the server (accept/session loops)
    /// is torn down by the runtime when the process exits.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.tick_task.await;
    }
}

/// How often the world is snapshotted to disk when new edits have landed
/// (whichever comes first; a clean shutdown always saves, so a crash costs
/// at most this much edit history).
const SAVE_INTERVAL: Duration = Duration::from_secs(5);
const SAVE_EVERY_EDITS: usize = 64;

/// How many player identities the rejoin registry keeps (persisted in the
/// world save). Evicted oldest-first by `last_seen` when exceeded: records
/// are a convenience, not a promise, and new identities keep arriving from
/// fresh browser profiles.
const MAX_PLAYER_RECORDS: usize = 64;

/// A token presented by a client that has no stored identity (the all-zero
/// "fresh identity" wire value). The server never mints it.
const NO_TOKEN: [u8; 16] = [0u8; 16];

/// Mint a fresh rejoin token: 16 random bytes from the OS (a capability,
/// not a credential — see `PlayerRegistry`). The all-zero value is
/// reserved for "no token", so mask it away (probability 2^-128 anyway).
fn mint_token() -> [u8; 16] {
    let mut t = [0u8; 16];
    getrandom::fill(&mut t).expect("OS entropy source available");
    if t == NO_TOKEN {
        t[0] = 1;
    }
    t
}

/// Unix seconds now (record ordering; a pre-epoch clock is clamped to 0).
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The rejoin registry: token → last-known player state (position, view,
/// name, colour). A player who presents a known token is restored to that
/// state ("reclaim the previous instance").
///
/// It lives inside [`WorldState`] (under the world lock) on purpose: the
/// periodic/final save snapshot is already taken under that lock, so the
/// identities ride the existing atomic-write / cadence / final-save
/// machinery with zero extra synchronisation. They are persisted in the
/// world save, which is bound to the seed — a token only works against the
/// world it was minted for ("same world only", with no auth to maintain).
#[derive(Default)]
struct PlayerRegistry {
    records: HashMap<[u8; 16], PlayerRecord>,
}

impl PlayerRegistry {
    /// Build from a decoded save (a duplicate token is last-wins — a set,
    /// like the override entries). The cap is re-enforced: a hand-edited
    /// file could exceed it.
    fn new(mut records: Vec<([u8; 16], PlayerRecord)>) -> Self {
        records.sort_unstable_by_key(|(_, r)| r.last_seen);
        let drop = records.len().saturating_sub(MAX_PLAYER_RECORDS);
        records.drain(..drop);
        let records = records.into_iter().collect();
        Self { records }
    }

    fn get(&self, token: &[u8; 16]) -> Option<&PlayerRecord> {
        self.records.get(token)
    }

    /// Record a player's final state under `token` (disconnect). Evicts
    /// the oldest record (min `last_seen`) when at capacity.
    fn upsert(&mut self, token: [u8; 16], record: PlayerRecord) {
        if !self.records.contains_key(&token) && self.records.len() >= MAX_PLAYER_RECORDS {
            // Copy the token out (owned) so the iteration borrow ends
            // before the removal.
            if let Some(oldest) = self
                .records
                .iter()
                .min_by_key(|(_, r)| r.last_seen)
                .map(|(t, _)| *t)
            {
                self.records.remove(&oldest);
            }
        }
        self.records.insert(token, record);
    }

    /// The complete set (save snapshot). Order is irrelevant (a set keyed
    /// by token); the caller may sort for a deterministic file if wanted.
    fn snapshot(&self) -> Vec<([u8; 16], PlayerRecord)> {
        self.records.iter().map(|(t, r)| (*t, r.clone())).collect()
    }

    fn len(&self) -> usize {
        self.records.len()
    }

    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Read and validate the world save file at `path` (None when absent).
///
/// Returns the block overrides plus the rejoin identities (v2 saves; v1
/// saves and absent files yield an empty list).
///
/// A save is bound to the seed that generated its terrain: a mismatched
/// seed means the file belongs to a different world, and loading it would
/// replay edits onto the wrong terrain. Fail fast with a clear message.
fn load_save(
    path: &std::path::Path,
    seed: u64,
) -> Result<(Option<Vec<Edit>>, Vec<([u8; 16], PlayerRecord)>), String> {
    if !path.exists() {
        return Ok((None, Vec::new()));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let (saved_seed, edits, players) = save::decode(&bytes)
        .map_err(|e| format!("corrupt save file {}: {e} — delete it to start a fresh world", path.display()))?;
    if saved_seed != seed {
        return Err(format!(
            "save file {} was created with seed {saved_seed}, but the server was started with seed {seed} — delete the save or start with the matching seed",
            path.display()
        ));
    }
    Ok((Some(edits), players))
}

/// Atomically replace `path` with `bytes`: write a unique temp file in the
/// same directory, fsync it, then rename over the target. The rename is
/// atomic on POSIX, so a reader (or a concurrent saver) only ever sees a
/// complete old or complete new save — never a torn one.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "world.save".to_string());
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    std::io::Write::write_all(&mut f, bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
    /// Rejoin registry: token → last-known player state (persisted in the
    /// world save — see [`PlayerRegistry`]).
    registry: PlayerRegistry,
    /// Identities currently connected (token → live player id). A second
    /// connection presenting a live token gets a FRESH identity instead of
    /// hijacking the connected player (a second tab with the same browser
    /// profile).
    active: HashMap<[u8; 16], u32>,
    /// A player record changed since the last successful save (the tick
    /// loop folds this into the save trigger, like new edits).
    players_dirty: bool,
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

    // Load the world save (if any) BEFORE binding: a corrupt or seed-
    // mismatched save must fail fast, not mid-listen. The save stores the
    // seed + the world's block overrides — terrain is a pure function of
    // the seed, so that pair is the world's entire persistent state.
    let save_path = opts.data_dir.join(save::SAVE_FILE_NAME);
    let (saved_edits, saved_players) = load_save(&save_path, opts.seed)?;

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
    let server = match &saved_edits {
        Some(edits) => {
            eprintln!(
                "[qwencraft-net] loaded {} saved block edits from {}",
                edits.len(),
                save_path.display()
            );
            Server::new_world_loaded(opts.seed, edits)
        }
        None => Server::new_world(opts.seed),
    };
    let registry = PlayerRegistry::new(saved_players);
    if !registry.is_empty() {
        eprintln!(
            "[qwencraft-net] loaded {} player identities from {} (rejoin ready)",
            registry.len(),
            save_path.display()
        );
    }
    let world = Arc::new(Mutex::new(WorldState {
        server,
        players: HashMap::new(),
        registry,
        active: HashMap::new(),
        players_dirty: false,
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
        match &saved_edits {
            Some(edits) => format!(
                "server started (seed {}, {scheme}://{addr}{WS_PATH}, {} saved edits loaded)",
                opts.seed,
                edits.len()
            ),
            None => format!("server started (seed {}, {scheme}://{addr}{WS_PATH})", opts.seed),
        },
    );

    // The single tick loop for the shared world (also owns the periodic
    // world save and the final save on clean shutdown).
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let tick_task = tokio::spawn(tick_loop(
        world.clone(),
        map.clone(),
        debug,
        save_path.clone(),
        opts.seed,
        stop_rx,
    ));
    let shutdown = ServerShutdown {
        stop: stop_tx,
        tick_task,
    };
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

    Ok(ServerEndpoints { addr, shutdown })
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
///
/// This loop owns the world save: when enough new edits have landed (or
/// the save interval elapsed) it snapshots the world's persistent state
/// (seed + block overrides) to `save_path` — encoded under the world lock,
/// written off the tick path via `spawn_blocking`, atomically replaced.
/// On a clean shutdown signal it awaits any in-flight save, takes one final
/// snapshot, and returns (see [`ServerShutdown::stop`]).
async fn tick_loop(
    world: Arc<Mutex<WorldState>>,
    map: Arc<Mutex<map::MapState>>,
    debug: bool,
    save_path: std::path::PathBuf,
    seed: u64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
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
    // World save state: how many edits the save file on disk covers, when
    // the last save was attempted, and an in-flight save (edit count it
    // covers + its blocking write handle).
    let mut disk_edits = world.lock().unwrap().server.world().edits().len();
    let mut last_save_try = Instant::now();
    let mut saving: Option<(usize, tokio::task::JoinHandle<std::io::Result<()>>)> = None;

    loop {
        // A clean shutdown signal breaks out (the final save runs below);
        // otherwise this yields at most one tick period.
        tokio::select! {
            _ = tick.tick() => {}
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow_and_update() {
                    break;
                }
            }
        }
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();

        // Reap a finished periodic save (a failed one leaves the on-disk
        // file at the last good snapshot — the trigger below retries).
        if let Some((count, h)) = saving.take() {
            if h.is_finished() {
                match h.await {
                    Ok(Ok(())) => {
                        disk_edits = count;
                        // The snapshot covered the player records as encoded
                        // (a disconnect since then sets the flag again and
                        // the next trigger saves it).
                        world.lock().unwrap().players_dirty = false;
                    }
                    Ok(Err(e)) => eprintln!("[qwencraft-net] world save failed: {e}"),
                    Err(e) => eprintln!("[qwencraft-net] world save task failed: {e}"),
                }
            } else {
                saving = Some((count, h));
            }
        }

        let mut w = world.lock().unwrap();
        // Destructure into disjoint field references so the per-connection
        // loop can touch `players` and `server` independently (the MutexGuard
        // itself doesn't field-split through its deref).
        let WorldState { server, players, registry, players_dirty, .. } = &mut *w;
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

        // Periodic world save: snapshot the world's persistent state (seed
        // + overrides + rejoin identities) when new edits have landed or a
        // player record changed, and either the interval elapsed or a big
        // enough batch accumulated. The snapshot is the COMPLETE set (not a
        // journal), so skipping/missing a save costs nothing beyond the
        // crash window.
        let n_edits = server.world().edits().len();
        if saving.is_none()
            && (n_edits > disk_edits || *players_dirty)
            && (n_edits.saturating_sub(disk_edits) >= SAVE_EVERY_EDITS
                || last_save_try.elapsed() >= SAVE_INTERVAL)
        {
            let overrides: Vec<Edit> = server.world().overrides().collect();
            let players = registry.snapshot();
            let bytes = save::encode(seed, &overrides, &players);
            let path = save_path.clone();
            saving = Some((
                n_edits,
                tokio::task::spawn_blocking(move || atomic_write(&path, &bytes)),
            ));
            last_save_try = Instant::now();
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

    // Clean shutdown: wait for any in-flight periodic save, then take one
    // final snapshot (it may predate the newest edits — the last periodic
    // save is only as fresh as the last tick that triggered it).
    if let Some((_, h)) = saving.take() {
        if let Err(e) = h.await {
            eprintln!("[qwencraft-net] world save task failed: {e}");
        }
    }
    let (n_edits, overrides, players, players_dirty) = {
        let mut w = world.lock().unwrap();
        // A clean stop must not lose the last state of still-connected
        // players (they will come back with their tokens): fold their
        // current state into the registry for the final snapshot — the
        // disconnect path does the same for players who leave first.
        let now = now_unix_secs();
        let live: Vec<([u8; 16], u32)> = w.active.iter().map(|(t, id)| (*t, *id)).collect();
        for (token, id) in live {
            let st = w.server.agent_state(id);
            w.registry.upsert(
                token,
                PlayerRecord {
                    pos: st.pos,
                    yaw: st.yaw,
                    pitch: st.pitch,
                    name: st.name,
                    color: st.color,
                    last_seen: now,
                },
            );
        }
        let world = w.server.world();
        (
            world.edits().len(),
            world.overrides().collect::<Vec<Edit>>(),
            w.registry.snapshot(),
            w.players_dirty || !w.active.is_empty(),
        )
    };
    // A clean stop always saves when there is anything unsaved — including
    // a player record that changed after the last edit snapshot (a
    // disconnect with no new edits since).
    if n_edits > disk_edits || players_dirty {
        let bytes = save::encode(seed, &overrides, &players);
        let path = save_path.clone();
        let write_path = path.clone();
        match tokio::task::spawn_blocking(move || atomic_write(&write_path, &bytes)).await {
            Ok(Ok(())) => {
                eprintln!(
                    "[qwencraft-net] world saved: {} edits, {} identities → {}",
                    n_edits,
                    players.len(),
                    path.display()
                );
            }
            Ok(Err(e)) => eprintln!("[qwencraft-net] final world save failed: {e}"),
            Err(e) => eprintln!("[qwencraft-net] final world save task failed: {e}"),
        }
    }
}

/// Apply one decoded client message to the shared world (input/actions are
/// stored per-player; the tick loop applies them on the next step).
fn apply_inbound(world: &Mutex<WorldState>, player_id: u32, m: ClientMsg) {
    let mut w = world.lock().unwrap();
    let WorldState { server, players, .. } = &mut *w;
    match m {
        ClientMsg::Input { keys, dx, dy, analog_x, analog_y } => {
            let mut input = Input::default();
            input.keys = KeySet::from_bits(keys);
            input.mouse_dx = dx;
            input.mouse_dy = dy;
            input.analog_x = analog_x;
            input.analog_y = analog_y;
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
        ClientMsg::Rejoin { .. } => {
            // `Rejoin` only has meaning as the FIRST frame (the identity
            // handshake, read before registration) — ignore it mid-session.
            eprintln!("[qwencraft-net] player {player_id}: mid-session Rejoin ignored");
        }
    }
}

/// Serve one connection: read the client's identity claim (the first-frame
/// `Rejoin` token — see the rejoin registry), register a player in the
/// shared world (restoring the claimed instance when recognised), forward
/// inbound client messages to it, and stream outbound server messages to
/// the socket. The world ticks on the shared [`tick_loop`]; this task only
/// moves data between the socket and the world.
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

    // Identity handshake BEFORE player registration: the client's first
    // frame is `Rejoin { token }` (all-zero token = fresh identity; a
    // stored token claims the previous instance). Reading it first lets a
    // rejoiner be restored IN PLACE (same spot / name / colour — same
    // world only, since the registry lives in the seed-bound save) instead
    // of spawning a throwaway player to swap out.
    //
    // A 2 s deadline: v8 clients send the frame within milliseconds of the
    // socket opening. A pre-v8 page sends nothing until it sees Hello, and
    // the version bump in that Hello is exactly what triggers its existing
    // cache-busting reload; silent or broken connections die here quickly.
    let first_msgs: Vec<ClientMsg> =
        match tokio::time::timeout(Duration::from_secs(2), rd.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => ClientMsg::decode_stream(&data).0,
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                return Err("connection closed before the identity handshake".to_string())
            }
            Ok(Some(Err(e))) => return Err(format!("handshake read: {e}")),
            Ok(Some(Ok(_))) => Vec::new(), // a non-binary frame: no identity claim
            Err(_) => Vec::new(), // timeout: proceed with a fresh identity (see above)
        };
    let mut first_iter = first_msgs.into_iter();
    let first_msg = first_iter.next();

    // Register: reclaim the claimed identity when recognised (restore the
    // recorded position — with the blocked-cell lift — view, name and
    // colour), mint a fresh token otherwise. `token` is what this
    // connection's Hello carries and what its disconnect snapshot is
    // stored under.
    let (player_id, token, rejoin_note) = {
        let mut w = world.lock().unwrap();
        let claimed = match first_msg.as_ref() {
            Some(ClientMsg::Rejoin { token }) if *token != NO_TOKEN => Some(*token),
            _ => None,
        };
        let (id, token, note) = match claimed {
            // The identity is already connected (a second tab with the same
            // browser profile): don't hijack the live player — this
            // connection gets a fresh identity.
            Some(t) if w.active.contains_key(&t) => {
                eprintln!(
                    "[qwencraft-net] {peer}: rejoin denied (identity already connected) — fresh identity"
                );
                (w.server.add_player(), mint_token(), None)
            }
            Some(t) => match w.registry.get(&t).cloned() {
                // REJOIN: the recorded instance of this world. (The codec
                // validates finiteness, so a restore failure is effectively
                // impossible; the spawn fallback keeps the identity anyway.)
                Some(rec) => {
                    let id = w.server.add_player();
                    let note =
                        match w.server.restore_agent(id, rec.pos, rec.yaw, rec.pitch) {
                            Ok(p) => Some(format!(
                                "restored at ({:.0}, {:.0}, {:.0})",
                                p.x, p.y, p.z
                            )),
                            Err(e) => {
                                eprintln!(
                                    "[qwencraft-net] {peer}: rejoin restore failed: {e} — spawn fallback"
                                );
                                None
                            }
                        };
                    w.server.set_profile(id, rec.name.clone(), rec.color);
                    (id, t, note)
                }
                // Unknown token (different world, evicted record, or a
                // forged token): fresh identity. The client overwrites its
                // stored token when it sees the new one in Hello.
                None => {
                    eprintln!(
                        "[qwencraft-net] {peer}: rejoin: unknown token — fresh identity"
                    );
                    (w.server.add_player(), mint_token(), None)
                }
            },
            // No claim: all-zero token, a missing first frame (timeout), or
            // a pre-v8 client that waits for Hello before speaking.
            None => (w.server.add_player(), mint_token(), None),
        };
        w.active.insert(token, id);
        w.players
            .insert(id, Conn { tx: tx.clone(), streamer: Streamer::new() });
        let verb = if note.is_some() { "rejoined" } else { "joined" };
        w.events.push(
            w.started.elapsed().as_secs_f64(),
            format!("player {id} {verb} (from {peer}, {} online)", w.players.len()),
        );
        (id, token, note)
    };
    let note_suffix = rejoin_note
        .as_deref()
        .map(|n| format!(" — {n}"))
        .unwrap_or_default();
    eprintln!(
        "[qwencraft-net] {peer}: player {player_id} {} (shared world seed {seed}, {} online){note_suffix}",
        if rejoin_note.is_some() { "rejoined" } else { "joined" },
        world.lock().unwrap().players.len()
    );

    // Any extra messages in the first frame (rare — one message per frame
    // in practice) apply now that the player exists.
    for m in first_iter {
        apply_inbound(&world, player_id, m);
    }

    // Hello (carries this connection's own player id so the client can
    // render the *other* players, plus its rejoin token to persist): the
    // client waits for it before sending input.
    let _ = tx.send(ServerMsg::Hello {
        version: PROTOCOL_VERSION,
        seed,
        player_id,
        token,
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
    // Deregister: persist this identity's final state (the rejoin record —
    // saved with the world, bound to its seed), then remove the player from
    // the shared world (the world and the other players remain).
    {
        let mut w = world.lock().unwrap();
        let st = w.server.agent_state(player_id);
        w.registry.upsert(
            token,
            PlayerRecord {
                pos: st.pos,
                yaw: st.yaw,
                pitch: st.pitch,
                name: st.name,
                color: st.color,
                last_seen: now_unix_secs(),
            },
        );
        w.active.remove(&token);
        w.players_dirty = true;
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

/// Unit tests for the rejoin registry (the world-save round-trip of
/// records is covered in `qwencraft-server::save`'s tests).
#[cfg(test)]
mod tests {
    use super::*;
    use qwencraft_world::Vec3;

    fn record(last_seen: u64, x: f32) -> PlayerRecord {
        PlayerRecord {
            pos: Vec3::new(x, 20.0, 8.5),
            yaw: 0.0,
            pitch: 0.0,
            name: format!("p{last_seen}"),
            color: [1, 2, 3],
            last_seen,
        }
    }

    #[test]
    fn minted_tokens_are_unique_and_never_zero() {
        let a = mint_token();
        let b = mint_token();
        assert_ne!(a, b);
        assert_ne!(a, NO_TOKEN);
        assert_ne!(b, NO_TOKEN);
    }

    #[test]
    fn upsert_inserts_and_updates() {
        let mut r = PlayerRegistry::default();
        r.upsert([1; 16], record(100, 1.0));
        assert_eq!(r.get(&[1; 16]).unwrap().last_seen, 100);
        // Same token: the record is UPDATED (a rejoiner's new final state),
        // not duplicated.
        r.upsert([1; 16], record(200, 2.0));
        assert_eq!(r.len(), 1);
        assert_eq!(r.get(&[1; 16]).unwrap().last_seen, 200);
        assert_eq!(r.get(&[1; 16]).unwrap().pos.x, 2.0);
    }

    #[test]
    fn upsert_evicts_oldest_at_capacity() {
        let mut r = PlayerRegistry::default();
        for i in 0..MAX_PLAYER_RECORDS {
            r.upsert([i as u8; 16], record(i as u64, 0.0));
        }
        assert_eq!(r.len(), MAX_PLAYER_RECORDS);
        // A new identity evicts the oldest (min last_seen = token 0).
        r.upsert([0xFF; 16], record(10_000, 0.0));
        assert_eq!(r.len(), MAX_PLAYER_RECORDS);
        assert!(r.get(&[0; 16]).is_none(), "oldest must be evicted");
        assert!(r.get(&[0xFF; 16]).is_some(), "newest must be kept");
        assert!(r.get(&[1; 16]).is_some());
    }

    #[test]
    fn new_from_save_dedups_and_caps() {
        // Duplicate token: last-wins (a set, like the override entries).
        let recs = vec![
            ([5u8; 16], record(10, 0.0)),
            ([5u8; 16], record(20, 0.0)),
        ];
        let r = PlayerRegistry::new(recs);
        assert_eq!(r.len(), 1);
        assert_eq!(r.get(&[5; 16]).unwrap().last_seen, 20);
        // Over-capacity input (a hand-edited file): oldest dropped.
        let recs: Vec<_> = (0..MAX_PLAYER_RECORDS + 10)
            .map(|i| ([i as u8; 16], record(i as u64, 0.0)))
            .collect();
        let r = PlayerRegistry::new(recs);
        assert_eq!(r.len(), MAX_PLAYER_RECORDS);
        assert!(r.get(&[0; 16]).is_none(), "10 oldest must be dropped");
        assert!(r.get(&[(MAX_PLAYER_RECORDS + 9) as u8; 16]).is_some());
    }

    #[test]
    fn snapshot_round_trips() {
        let mut r = PlayerRegistry::default();
        r.upsert([1; 16], record(1, 1.5));
        r.upsert([2; 16], record(2, 2.5));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 2);
        let r2 = PlayerRegistry::new(snap);
        assert_eq!(r2.get(&[1; 16]).unwrap().pos.x, 1.5);
        assert_eq!(r2.get(&[2; 16]).unwrap().pos.x, 2.5);
    }
}
