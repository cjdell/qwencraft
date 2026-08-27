#!/usr/bin/env bash
# Build the server dashboard (a standalone dioxus wasm workspace under
# dashboard/) into dashboard/dist/ — the tree is embedded into the
# rustcraft-net binary via include_dir!, so after running this script the
# server must be rebuilt (cargo build -p rustcraft-net).
#
# The dist tree is committed: the server binary has no filesystem
# dependencies at runtime, and CI/other machines get the exact same assets
# without a wasm toolchain.
set -euo pipefail
cd "$(dirname "$0")/.."

DASH=dashboard
TARGET=$DASH/target/wasm32-unknown-unknown/release

echo "== building dashboard (wasm32) =="
(
  cd "$DASH"
  cargo build --release --target wasm32-unknown-unknown
)

# wasm-bindgen must match the dashboard's wasm-bindgen crate exactly
# (0.2.100, the same pin as the main app — see AGENTS.md: a mismatch
# produces a broken bundle that fails at load with "unsupported version").
echo "== wasm-bindgen =="
CRATE_VERSION=$(
  awk '/^name = "wasm-bindgen"$/{getline; if ($0 ~ /^version/) {sub(/version = "/,"",$0); sub(/".*/,"",$0); print; exit}}' \
    "$DASH/Cargo.lock"
)
CLI_VERSION=$(wasm-bindgen --version | awk '{print $2}')
echo "   crate: $CRATE_VERSION  cli: $CLI_VERSION"
if [ "$CRATE_VERSION" != "$CLI_VERSION" ]; then
  echo "error: wasm-bindgen CLI ($CLI_VERSION) != crate ($CRATE_VERSION)" >&2
  exit 1
fi

rm -rf "$DASH/dist"
# cdylib artifact (underscored, lib-style naming — the old bin build used
# the hyphenated name).
wasm-bindgen --target web --out-dir "$DASH/dist" \
  "$TARGET/rustcraft_dashboard.wasm"

echo "== copying html/css into dist =="
cp "$DASH/index.html" "$DASH/dist/index.html"
cp "$DASH/style.css" "$DASH/dist/dashboard.css"

ls -la "$DASH/dist"
echo
echo "done. Rebuild the server to pick up the new assets:"
echo "  cargo build -p rustcraft-net"
