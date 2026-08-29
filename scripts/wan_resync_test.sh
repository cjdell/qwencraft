#!/usr/bin/env bash
# Deterministic TRANSIT-LOSS test for the remote backend's resync repair
# (protocol v5). A TLS-terminating WAN proxy (wan_proxy.py: burst RTT +
# optional rate limit, one thread per connection) sits between headless
# Chromium and the headless server and DROPS ~4 MB of whole WS frames from
# the middle of the initial chunk burst (after the first ~80 KB get
# through) — the failure mode that leaves the spawn view "floating in
# space" on flaky internet connections.
#
# Pre-fix: chunks lost before the client ever saw them are invisible to
# eviction reporting (the streamer's sent set has no way to know); only a
# block edit in the missing area forced a re-send. Post-fix: the client
# detects the gap (server's per-viewer chunks_sent far beyond the distinct
# chunks it actually holds, and none arriving for 5 s), reports the chunks
# it has (ClientMsg::Resync), and the server re-sends the rest — the view
# fills in with no user action.
#
# No --virtual-time-budget (like remote_test.sh): the resync timers are
# real-time based (Date.now()) and a live 60 Hz socket never quiesces; we
# wait in wall clock instead.
#
# Run inside `nix develop`. Expects ./scripts/build.sh to have been run.
set -uo pipefail
cd "$(dirname "$0")/.."
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

RTT_MS="${RTT_MS:-200}"
SEED="${SEED:-1337}"
PORT="${PORT:-$((20000 + RANDOM % 20000))}"
WS_PORT="${WS_PORT:-$((40000 + RANDOM % 20000))}"
PROXY_PORT="${PROXY_PORT:-$((40000 + RANDOM % 20000))}"
# Wall-clock seconds the page runs: connect (~3 s) + initial stream (~4 s)
# + the 5 s stale window + resync + re-stream; 90 s leaves wide margin.
RUN_SECS="${RUN_SECS:-90}"
LOG="${LOG:-${TMPDIR:-/tmp}/qwencraft-wan-chrome.log}"
WS_LOG="${WS_LOG:-${TMPDIR:-/tmp}/qwencraft-wan-server.log}"
PROXY_LOG="${PROXY_LOG:-${TMPDIR:-/tmp}/qwencraft-wan-proxy.log}"
PROF_DIR="${TMPDIR:-/tmp}/qwencraft-wan-prof"
rm -rf "$PROF_DIR"; mkdir -p "$PROF_DIR"

[ -f web/dist/index.html ] || { echo "web/dist not found — run ./scripts/build.sh first" >&2; exit 1; }

# Headless server binary. It EMBEDS web/dist via include_dir! (not
# fingerprinted by cargo) — a binary built before the latest web/dist
# serves a stale client (old protocol / no resync logic), so force a
# rebuild when web/dist is newer.
SRV_BIN=target/release/qwencraft-net
if [ ! -x "$SRV_BIN" ] || [ -n "$(find web/dist -newer "$SRV_BIN" -print -quit 2>/dev/null)" ]; then
  echo "==> building qwencraft-net (release)"
  [ -e web/dist ] && touch crates/qwencraft-net/src/lib.rs
  cargo build --release -p qwencraft-net || exit 1
fi

# Find a lavapipe (software Vulkan) ICD — see verify.sh for the rationale.
VK_ICD=""
for d in /nix/store/*/share/vulkan/icd.d; do
  for f in "$d"/*.json; do
    if [ -f "$f" ] && grep -qi "lvp\|swrast" "$f" 2>/dev/null; then VK_ICD="$f"; break 2; fi
  done
done
export VK_ICD_FILENAMES="${VK_ICD:-}"

echo "==> transit-loss test: server :${WS_PORT}  proxy :${PROXY_PORT} (rtt=${RTT_MS}ms, drop 4MB of frames after 80KB)  page :${PORT}"
"$SRV_BIN" --seed "$SEED" --port "$WS_PORT" --bind 127.0.0.1 --debug >"$WS_LOG" 2>&1 &
SRV=$!
openssl req -x509 -newkey rsa:2048 -keyout "$PROF_DIR/wan.key" -out "$PROF_DIR/wan.crt" -days 1 -nodes -subj "/CN=127.0.0.1" >/dev/null 2>&1
python3 -m http.server "$PORT" --directory web/dist --bind 127.0.0.1 >/dev/null 2>&1 &
HTTP=$!
DROP_AFTER_BYTES=80000 DROP_BYTES=4000000 \
  python3 "$SCRIPT_DIR/wan_proxy.py" "$PROXY_PORT" "$PROF_DIR/wan.crt" "$PROF_DIR/wan.key" 127.0.0.1 "$WS_PORT" "$RTT_MS" 0 >"$PROXY_LOG" 2>&1 &
PROXY=$!
cleanup() { kill "$SRV" "$HTTP" "$PROXY" 2>/dev/null || true; }
trap cleanup EXIT
sleep 1
for _ in $(seq 1 50); do grep -q "qwencraft-net: ready" "$WS_LOG" 2>/dev/null && break; sleep 0.2; done
grep -q "qwencraft-net: ready" "$WS_LOG" || { echo "FAIL: server did not start"; cat "$WS_LOG"; exit 1; }

timeout "$RUN_SECS" chromium \
  --headless --no-sandbox --disable-gpu-sandbox \
  --enable-unsafe-webgpu --enable-unsafe-swiftshader --use-angle=swiftshader \
  --ignore-certificate-errors \
  --user-data-dir="$PROF_DIR" --window-size=1280,720 \
  --enable-logging=stderr --v=0 \
  "http://127.0.0.1:${PORT}/?seed=${SEED}&server=wss://127.0.0.1:${PROXY_PORT}/ws&dbg=1" \
  >"$LOG" 2>&1

fail=0
check() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then
    echo "PASS: $desc"
  else
    echo "FAIL: $desc"
    fail=1
  fi
}

echo
echo "== POOL series =="
POOL_LINES=$(grep -o "POOL chunks=[0-9]* missing=[0-9]* sent=[0-9]*" "$LOG")
echo "$POOL_LINES"
LAST=$(echo "$POOL_LINES" | tail -1 | grep -o "chunks=[0-9]*" | cut -d= -f2)
LAST=${LAST:-0}

check "client connected"        grep -q "remote server connected" "$LOG"
check "proxy dropped frames"    grep -Eq "total 4[0-9]{6}/" "$PROXY_LOG"
check "client requested resync" grep -q "requesting resync" "$LOG"
check "server re-sent the gap"  grep -q "resync — [0-9]* chunk regions re-sent" "$WS_LOG"
# The view must RECOVER: a full spawn view holds ~328 pool chunks; without
# the resync the client is stuck at whatever survived the drop (~175).
if [ "$LAST" -ge 250 ]; then
  echo "PASS: view recovered (final pool chunks=${LAST} >= 250)"
else
  echo "FAIL: view did not recover (final pool chunks=${LAST} < 250)"
  fail=1
fi
echo
echo "== resync log lines =="
grep -E "requesting resync" "$LOG" | head -2
grep -E "resync — " "$WS_LOG" | head -2
grep -E "dropped" "$PROXY_LOG" | tail -1

echo
if [ "$fail" = 0 ]; then
  echo "TRANSIT-LOSS TEST PASSED (log: $LOG, server: $WS_LOG, proxy: $PROXY_LOG)"
else
  echo "TRANSIT-LOSS TEST FAILED (log: $LOG, server: $WS_LOG, proxy: $PROXY_LOG)"
fi
exit $fail
