//! Headless RustCraft server binary.
//!
//! ```text
//! rustcraft-net [--seed N] [--port N] [--bind IP] [--cert FILE --key FILE]
//! ```
//!
//! Serves one world per WebSocket connection (see `rustcraft-net` lib docs).
//! Browser clients point at it with `?server=ws://host:port` (or the in-page
//! connect panel).
//!
//! The server is host-only (tokio/mio don't support wasm); the wasm stub at
//! the bottom keeps the shared workspace's wasm build green.

#[cfg(not(target_arch = "wasm32"))]
const USAGE: &str = "\
usage: rustcraft-net [options]
  --seed N     world seed (default 1337)
  --port N     listen port (default 9000; 0 = let the OS pick)
  --bind IP    interface to bind (default 0.0.0.0)
  --cert FILE  TLS certificate (PEM) — with --key, serves wss://
  --key FILE   TLS private key (PEM, RSA or PKCS#8)
  -h, --help   this help";

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
fn parse_opts(args: &[String]) -> Result<rustcraft_net::ServerOptions, String> {
    use std::net::IpAddr;

    let mut opts = rustcraft_net::ServerOptions::default();
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
            eprintln!("rustcraft-net: {msg}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match rustcraft_net::serve(opts).await {
        Ok(addr) => {
            eprintln!("rustcraft-net: ready (connect a browser to ws://{addr})");
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("rustcraft-net: shutting down");
        }
        Err(e) => {
            eprintln!("rustcraft-net: {e}");
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
