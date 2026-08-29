//! Headless Qwencraft server binary.
//!
//! ```text
//! qwencraft-net [--seed N] [--port N] [--bind IP] [--cert FILE --key FILE]
//! ```
//!
//! One port hosts everything: the game WebSocket at `/ws`, the operator
//! dashboard at `/dashboard`, and (when `web/dist` was present at build
//! time) the game client itself at `/` — so the whole game can be hosted on
//! a single authority. Serves one **shared world** for all WebSocket
//! connections: every client that connects joins the same world and can see
//! the other players and their edits (see `qwencraft-net` lib docs).
//! Browser clients point at it with `?server=ws://host:port/ws` (or the
//! in-page options panel); open two browsers at the same URL to play
//! together.
//!
//! The server is host-only (tokio/mio don't support wasm); the wasm stub at
//! the bottom keeps the shared workspace's wasm build green.

#[cfg(not(target_arch = "wasm32"))]
const USAGE: &str = "\
usage: qwencraft-net [options]
  --seed N       world seed (default 1337). Must match the seed of an
                 existing save file in --data-dir (the save is bound to the
                 seed that generated its terrain).
  --port N       listen port (default 9000; 0 = let the OS pick). One port
                 hosts everything: WebSocket at /ws, dashboard at /dashboard,
                 game client at /
  --bind IP      interface to bind (default 0.0.0.0)
  --cert FILE    TLS certificate (PEM) — with --key, the port speaks
                 wss:// + https://
  --key FILE     TLS private key (PEM, RSA or PKCS#8)
  --data-dir DIR directory for the world save file (default ./data; the
                 save is DIR/world.save). The world's block edits are
                 snapshotted there periodically and on clean shutdown;
                 on start, an existing save is replayed into the fresh
                 terrain.
  --debug        log per-second per-player streaming telemetry to stderr
  -h, --help     this help";

#[cfg(not(target_arch = "wasm32"))]
fn next_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    let v = args
        .get(*i)
        .cloned()
        .ok_or_else(|| format!("missing value for {flag} (see --help)"))?;
    *i += 1;
    Ok(v)
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_opts(args: &[String]) -> Result<qwencraft_net::ServerOptions, String> {
    use std::net::IpAddr;

    let mut opts = qwencraft_net::ServerOptions::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--seed" => opts
                .seed = next_value(args, &mut i, "--seed")?
                .parse()
                .map_err(|e| format!("--seed: {e}"))?,
            "--port" => opts
                .port = next_value(args, &mut i, "--port")?
                .parse()
                .map_err(|e| format!("--port: {e}"))?,
            "--bind" => opts
                .bind = next_value(args, &mut i, "--bind")?
                .parse::<IpAddr>()
                .map_err(|e| format!("--bind: {e}"))?,
            "--cert" => {
                opts.cert = Some(std::path::PathBuf::from(next_value(args, &mut i, "--cert")?))
            }
            "--key" => {
                opts.key = Some(std::path::PathBuf::from(next_value(args, &mut i, "--key")?))
            }
            "--data-dir" => {
                opts.data_dir =
                    std::path::PathBuf::from(next_value(args, &mut i, "--data-dir")?)
            }
            "--debug" => opts.debug = true,
            other => return Err(format!("unknown option {other:?} (see --help)")),
        }
    }
    Ok(opts)
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() {
    let opts = match parse_opts(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("qwencraft-net: {msg}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let tls = opts.cert.is_some();
    match qwencraft_net::serve(opts).await {
        Ok(endpoints) => {
            let a = endpoints.addr;
            let (ws, http) = if tls { ("wss", "https") } else { ("ws", "http") };
            eprintln!("qwencraft-net: ready (connect a browser to {ws}://{a}{})", qwencraft_net::WS_PATH);
            eprintln!("qwencraft-net: dashboard at {http}://{a}/dashboard/ · game client at {http}://{a}/");
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("qwencraft-net: shutting down (saving world)");
            // Clean stop: the tick loop takes a final world save before it
            // returns; only then does the runtime tear the rest down.
            endpoints.shutdown.stop().await;
        }
        Err(e) => {
            eprintln!("qwencraft-net: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    // The headless server is host-only; this stub exists so the shared
    // workspace's `--target wasm32-unknown-unknown` build stays green.
    std::process::exit(1);
}
