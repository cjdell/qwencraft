#!/usr/bin/env bash
# Serve web/dist on http://localhost:8080 (override with PORT=...).
set -euo pipefail
cd "$(dirname "$0")/.."
PORT="${PORT:-8080}"
if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi
echo "Serving RustCraft on http://localhost:${PORT}  (seed via ?seed=123)"
exec python3 -m http.server "$PORT" --directory web/dist --bind 0.0.0.0
