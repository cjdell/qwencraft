#!/usr/bin/env bash
# Build the Qwencraft web app (wasm) into web/dist.
# Run inside `nix develop`.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo build (wasm32-unknown-unknown, release)"
# The `verify` feature carries the WebGL2 shadow renderer that the headless
# test harnesses (verify.sh, remote_test.sh) read back pixels through; a
# production build can drop it.
cargo build --release --target wasm32-unknown-unknown -p qwencraft-web --features verify

echo "==> wasm-bindgen"
rm -rf web/dist
mkdir -p web/dist/pkg
wasm-bindgen --target web --out-dir web/dist/pkg \
  target/wasm32-unknown-unknown/release/qwencraft_web.wasm

cp web/index.html web/dist/index.html
echo "==> done: web/dist (serve it with ./scripts/serve.sh)"
