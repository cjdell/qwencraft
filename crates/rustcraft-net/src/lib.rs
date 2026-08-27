//! Headless RustCraft server.
//!
//! Serves the authoritative [`rustcraft_server::Server`] over WebSocket
//! (`ws://`, or `wss://` with `--cert/--key`). The wire protocol is
//! [`rustcraft_server::protocol`]: the client sends input/actions, the server
//! sends player/agent state, chunk regions and stats at a fixed 60 Hz.
//!
//! **One world per connection** (mirrors the embedded single-player model:
//! each page has its own `Server`). A shared-world multiplayer server is a
//! later milestone; nothing in this crate precludes it.
//!
//! Host-only (tokio/mio don't support wasm): the whole crate compiles away
//! for wasm so the shared workspace's wasm build stays green.

#![cfg(not(target_arch = "wasm32"))]

use std::fs::File;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rustcraft_server::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use rustcraft_server::{Action, Input, KeySet, Server, WorldUpdate, TICK_HZ};
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
    /// Port to listen on (0 = let the OS pick; see [`serve`]).
    pub port: u16,
    /// TLS certificate (PEM). With [`Self::key`], the server speaks wss://.
    pub cert: Option<PathBuf>,
    /// TLS private key (PEM, RSA or PKCS#8).
    pub key: Option<PathBuf>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            seed: 1337,
            bind: "0.0.0.0".parse().unwrap(),
            port: 9000,
            cert: None,
            key: None,
        }
    }
}

/// Load the TLS acceptor, bind the listener, and spawn the accept loop.
///
/// Returns the actual bound address (pass `port: 0` to let the OS pick one —
/// used by the end-to-end test). The loop keeps running for the life of the
/// current tokio runtime.
pub async fn serve(opts: ServerOptions) -> Result<SocketAddr, String> {
    let tls = load_tls(&opts)?;
    let listener = TcpListener::bind((opts.bind, opts.port))
        .await
        .map_err(|e| format!("bind {}:{}: {e}", opts.bind, opts.port))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    eprintln!(
        "[rustcraft-net] listening on {scheme}://{addr} (seed {}, one world per connection)",
        opts.seed
    );
    let seed = opts.seed;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp, peer)) => {
                    let tls = tls.clone();
                    tokio::spawn(async move {
                        match handle_conn(tcp, peer, seed, tls).await {
                            Ok(()) => {}
                            Err(e) => eprintln!("[rustcraft-net] {peer}: {e}"),
                        }
                    });
                }
                Err(e) => eprintln!("[rustcraft-net] accept failed: {e}"),
            }
        }
    });
    Ok(addr)
}

/// WebSocket handshake (optionally over TLS), then the per-connection world.
async fn handle_conn(
    tcp: TcpStream,
    peer: SocketAddr,
    seed: u64,
    tls: Option<Arc<tokio_rustls::server::TlsAcceptor>>,
) -> Result<(), String> {
    match tls {
        Some(acceptor) => {
            let stream = acceptor
                .accept(tcp)
                .await
                .map_err(|e| format!("TLS handshake: {e}"))?;
            let ws = tokio_tungstenite::accept_async(stream)
                .await
                .map_err(|e| format!("WebSocket handshake: {e}"))?;
            session(ws, peer, seed).await
        }
        None => {
            let ws = tokio_tungstenite::accept_async(tcp)
                .await
                .map_err(|e| format!("WebSocket handshake: {e}"))?;
            session(ws, peer, seed).await
        }
    }
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

/// One decoded client message, or the reader giving up (close/EOF/error).
enum Inbound {
    Msg(ClientMsg),
    Closed,
}

/// Serve one connection: a world, a 60 Hz tick loop, and the wire both ways.
///
/// The read half runs in its own task (decoded messages arrive over a
/// channel); the write half stays with the tick loop, which also owns the
/// `Server`. `select!` over (channel, tick) keeps input latency at one tick
/// while the world runs on a steady 60 Hz regardless of client rate.
async fn session<S>(
    ws: WebSocketStream<S>,
    peer: SocketAddr,
    seed: u64,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    eprintln!("[rustcraft-net] {peer}: client connected (world seed {seed})");
    let mut server = Server::new(seed);
    let (mut wr, mut rd): (
        SplitSink<WebSocketStream<S>, Message>,
        SplitStream<WebSocketStream<S>>,
    ) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Inbound>();

    let reader = tokio::spawn(async move {
        while let Some(frame) = rd.next().await {
            match frame {
                // tungstenite answers pings automatically (protocol layer),
                // so nothing to do for control frames.
                Ok(Message::Binary(data)) => {
                    for m in ClientMsg::decode_stream(&data).0 {
                        if tx.send(Inbound::Msg(m)).is_err() {
                            return;
                        }
                    }
                }
                Ok(Message::Close(_)) | Ok(Message::Text(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(Inbound::Closed);
    });

    // Hello first: the client waits for it before sending input.
    send(&mut wr, &ServerMsg::Hello { version: PROTOCOL_VERSION, seed }).await?;

    // Latest input snapshot (the client sends one per frame; the tick
    // applies the most recent).
    let mut pending_input: Option<(u32, f32, f32)> = None;
    let mut pending_actions: Vec<Action> = Vec::new();
    let mut last_npc_load = server.npc_load_config();
    let mut last = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / TICK_HZ as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break; };
                match event {
                    Inbound::Msg(ClientMsg::Input { keys, dx, dy }) => {
                        pending_input = Some((keys, dx, dy));
                    }
                    Inbound::Msg(ClientMsg::Action(a)) => pending_actions.push(a),
                    Inbound::Msg(ClientMsg::ResendChunk(pos)) => {
                        // Pool eviction: re-send the region immediately (it
                        // is already generated; the server keeps everything).
                        if let Some(data) = server.resend_chunk(pos) {
                            send(&mut wr, &ServerMsg::Chunk { pos, data }).await?;
                        }
                    }
                    Inbound::Msg(ClientMsg::SetNpcLoad { count, spacing }) => {
                        server.set_npc_load(count, spacing);
                        pending_actions.push(Action::NpcLoad);
                    }
                    Inbound::Closed => break,
                }
            }
            _ = tick.tick() => {
                let dt = last.elapsed().as_secs_f64();
                last = Instant::now();

                if let Some((keys, dx, dy)) = pending_input {
                    let mut input = Input::default();
                    input.keys = KeySet::from_bits(keys);
                    input.mouse_dx = dx;
                    input.mouse_dy = dy;
                    server.set_input(input);
                }
                for a in pending_actions.drain(..) {
                    server.push_action(a);
                }
                server.tick(dt);

                for u in server.take_world_updates() {
                    match u {
                        WorldUpdate::Chunk { pos, data } => {
                            send(&mut wr, &ServerMsg::Chunk { pos, data }).await?;
                        }
                    }
                }
                send(&mut wr, &ServerMsg::PlayerState(server.player_state())).await?;
                send(&mut wr, &ServerMsg::Agents(server.agents())).await?;
                send(&mut wr, &ServerMsg::Stats(server.stats())).await?;
                let load = server.npc_load_config();
                if load != last_npc_load {
                    last_npc_load = load;
                    send(&mut wr, &ServerMsg::NpcLoad { count: load.0, spacing: load.1 }).await?;
                }
            }
        }
    }

    reader.abort();
    eprintln!("[rustcraft-net] {peer}: client disconnected");
    Ok(())
}

/// Encode and send one server message.
async fn send<S>(
    wr: &mut SplitSink<WebSocketStream<S>, Message>,
    msg: &ServerMsg,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    wr.send(Message::Binary(msg.encode().into()))
        .await
        .map_err(|e| format!("send: {e}"))
}
