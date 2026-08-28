#!/usr/bin/env bash
# Build the Qwencraft web app (wasm) into web/dist.
# Run inside `nix develop`.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo build (wasm32-unknown-unknown, release)"
cargo build --release --target wasm32-unknown-unknown -p qwencraft-web

echo "==> wasm-bindgen"
rm -rf web/dist
mkdir -p web/dist/pkg
wasm-bindgen --target web --out-dir web/dist/pkg \
  target/wasm32-unknown-unknown/release/qwencraft_web.wasm

cp web/index.html web/dist/index.html
echo "==> done: web/dist (serve it with ./scripts/serve.sh)"
