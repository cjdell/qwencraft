//! Build-time glue for the embedded game client.
//!
//! `http.rs` embeds `web/dist` (the wasm game client) via `include_dir!` so
//! the server can host the game on the same port as the WebSocket.
//! `web/dist` is a build artifact (gitignored, produced by
//! `./scripts/build.sh`), so on a fresh checkout it may not exist yet and
//! `include_dir!` would panic. This script materializes a minimal fallback
//! index page in that case (the gitignored directory stays a scratch
//! location; a real `./scripts/build.sh` output replaces it next build).

use std::path::PathBuf;

const FALLBACK_INDEX: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>RustCraft server</title></head>
<body style="font-family:monospace;background:#10141c;color:#dfe7f2;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
<div style="max-width:520px;line-height:1.6">
<h1>RustCraft server</h1>
<p>The game client was not embedded in this binary (build <code>web/dist</code>
with <code>./scripts/build.sh</code> and rebuild the server to play from this
port).</p>
<p><a style="color:#7fb3ff" href="/dashboard/">Open the server dashboard &rarr;</a></p>
</div>
</body></html>
"#;

fn main() {
    let dist = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("..")
        .join("web")
        .join("dist");
    println!("cargo:rerun-if-changed=../../web/dist");
    if !dist.join("index.html").exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(dist.join("index.html"), FALLBACK_INDEX);
        println!("cargo:warning=web/dist missing - serving a fallback page at / (run ./scripts/build.sh to embed the game client)");
    }
}
