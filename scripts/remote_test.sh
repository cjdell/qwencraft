#!/usr/bin/env bash
# Headless-browser test for REMOTE mode: the page connects to a standalone
# headless server (rustcraft-net) over WebSocket and renders its world.
#
# - builds the rustcraft-net binary (release) if missing,
# - starts the headless server + a static server for web/dist,
# - runs headless Chromium with ?server=ws://127.0.0.1:PORT&verify=1,
# - checks: app started, remote connection established, renderer ready,
#   first frame rendered, GPU pixel readback (sky + terrain — the scene is
#   rendered from server-streamed chunks), no uncaught JS errors, and the
#   server log shows a client connecting.
#
# Run inside `nix develop`. Expects ./scripts/build.sh to have been run.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-$((20000 + RANDOM % 20000))}"
WS_PORT="${WS_PORT:-$((40000 + RANDOM % 20000))}"
SEED="${SEED:-1337}"
LOG="${LOG:-${TMPDIR:-/tmp}/rustcraft-remote-chrome.log}"
LOG2="${LOG2:-${TMPDIR:-/tmp}/rustcraft-remote-chrome2.log}"
WS_LOG="${WS_LOG:-${TMPDIR:-/tmp}/rustcraft-remote-server.log}"
# Wall-clock seconds the page is left to connect, stream, and run the
# WebGL2 shadow readback (the app renders at ~60 fps real time; the
# readback fires at frame 410, ~7 s after the first frame).
RUN_SECS="${RUN_SECS:-45}"
# A second browser joins the SAME shared world concurrently (the multi-
# player scenario): it runs for this many seconds and must connect and be
# seen by the server alongside the first client.
SECOND_SECS="${SECOND_SECS:-25}"

# NOTE: no --virtual-time-budget here. Chromium's virtual-time
# fast-forward stalls against a live WebSocket (the server streams at a
# fixed 60 Hz real time, so the socket never quiesces and the virtual
# clock — with it, the page's JS — freezes). Remote mode is real-time by
# nature; we simply wait wall-clock seconds.

if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi

# Headless server binary (built once, reused afterwards).
SRV_BIN=target/release/rustcraft-net
if [ ! -x "$SRV_BIN" ]; then
  echo "==> building rustcraft-net (release)"
  cargo build --release -p rustcraft-net || exit 1
fi

# Find a lavapipe (software Vulkan) ICD — see verify.sh for the rationale.
VK_ICD=""
for d in /nix/store/*/share/vulkan/icd.d; do
  for f in "$d"/*.json; do
    if [ -f "$f" ] && grep -qi "lvp\|swrast" "$f" 2>/dev/null; then
      VK_ICD="$f"
      break 2
    fi
  done
done
export VK_ICD_FILENAMES="${VK_ICD:-}"
export TMPDIR="${TMPDIR:-/tmp}"
PROF_DIR="${TMPDIR}/rustcraft-remote-chrome-prof"
rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"

echo "==> headless server on ws://127.0.0.1:${WS_PORT} (seed ${SEED})"
"$SRV_BIN" --seed "$SEED" --port "$WS_PORT" --bind 127.0.0.1 >"$WS_LOG" 2>&1 &
SRV=$!
python3 -m http.server "$PORT" --directory web/dist --bind 127.0.0.1 >/dev/null 2>&1 &
HTTP=$!
cleanup() { kill "$SRV" "$HTTP" 2>/dev/null || true; }
trap cleanup EXIT
sleep 1

for _ in $(seq 1 50); do
  grep -q "rustcraft-net: ready" "$WS_LOG" 2>/dev/null && break
  sleep 0.2
done
if ! grep -q "rustcraft-net: ready" "$WS_LOG" 2>/dev/null; then
  echo "FAIL: headless server did not start" >&2
  cat "$WS_LOG" >&2
  exit 1
fi

# Second browser: joins the same shared world concurrently (a second
# profile dir, no pixel readback — it just has to connect and be seen).
PROF_DIR2="${TMPDIR}/rustcraft-remote-chrome-prof2"
rm -rf "$PROF_DIR2"
mkdir -p "$PROF_DIR2"
echo "==> second browser (same shared world, ${SECOND_SECS}s real time, background)"
timeout "$SECOND_SECS" chromium \
  --headless \
  --no-sandbox \
  --disable-gpu-sandbox \
  --enable-unsafe-webgpu \
  --enable-unsafe-swiftshader \
  --use-angle=swiftshader \
  --user-data-dir="$PROF_DIR2" \
  --window-size=1280,720 \
  --enable-logging=stderr --v=0 \
  "http://127.0.0.1:${PORT}/?seed=${SEED}&server=ws://127.0.0.1:${WS_PORT}" \
  >"$LOG2" 2>&1 &
SECOND=$!

echo "==> headless chromium (remote mode, ?server=ws://127.0.0.1:${WS_PORT}, ${RUN_SECS}s real time)"
timeout "$RUN_SECS" chromium \
  --headless \
  --no-sandbox \
  --disable-gpu-sandbox \
  --enable-unsafe-webgpu \
  --enable-unsafe-swiftshader \
  --use-angle=swiftshader \
  --user-data-dir="$PROF_DIR" \
  --window-size=1280,720 \
  --enable-logging=stderr --v=0 \
  "http://127.0.0.1:${PORT}/?seed=${SEED}&server=ws://127.0.0.1:${WS_PORT}&verify=1" \
  >"$LOG" 2>&1 || true

# Let the second browser finish (it runs shorter than the first).
wait "$SECOND" 2>/dev/null || true

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

check "app started"                    grep -q "RustCraft: app started" "$LOG"
check "remote server connected"        grep -q "RustCraft: remote server connected (seed ${SEED})" "$LOG"
check "renderer ready (WebGPU)"        grep -q "RustCraft: renderer ready" "$LOG"
check "first frame rendered"           grep -q "RustCraft: first frame rendered" "$LOG"
check "no uncaught JS errors"          bash -c "! grep -E 'Uncaught|TypeError|ReferenceError' '$LOG' | grep -v 'favicon' | grep -q ."
check "server saw the client"          grep -q "player 0 joined" "$WS_LOG"
# The second browser must have joined the SAME shared world while the first
# was connected (the server reports the online count on each join).
check "second browser connected"       grep -q "RustCraft: remote server connected (seed ${SEED})" "$LOG2"
check "shared world saw two players"   grep -q "2 online" "$WS_LOG"

# GPU readback: the shadow renderer re-renders the scene built from the
# server-streamed chunk regions; the 4x3 grid must show sky + terrain.
VP=$(grep -o "VERIFY_PIXELS .*" "$LOG" | tail -1 | sed 's/VERIFY_PIXELS //')
if [ -z "$VP" ]; then
  echo "FAIL: no GPU pixel readback in log"
  fail=1
else
  python3 - "$VP" <<'PY'
import sys
raw = sys.argv[1].strip().strip('"').rstrip(";").strip()
regs = [tuple(map(int, p.split(","))) for p in raw.split(";") if p.strip().replace(",", "").isdigit()]
assert len(regs) == 12, f"expected 12 regions, got {len(regs)}"
top = regs[0:4]
sky = sum(1 for (r, g, b) in top if b > r + 10 and r > 60)
assert sky >= 2, f"top row should be mostly sky: {top}"
bottom = regs[8:12]
green = sum(1 for (r, g, b) in bottom if g >= r and g > b + 5)
water = sum(1 for (r, g, b) in bottom if b > g > r and r < 125 and b > 100)
assert green + water >= 2, f"no terrain (grass or water) regions in bottom row: {bottom}"
distinct = len(set(regs))
assert distinct >= 4, f"scene looks flat, only {distinct} distinct region colours"
print(f"  GPU readback: 12 regions, top={top[0]} bottom={bottom[0]}, distinct={distinct}")
print("PASS: GPU pixel readback shows sky + terrain")
PY
  [ $? -eq 0 ] || fail=1
fi

echo
if [ "$fail" = 0 ]; then
  echo "ALL CHECKS PASSED (remote mode; log: $LOG, server log: $WS_LOG)"
else
  echo "SOME CHECKS FAILED (log: $LOG, server log: $WS_LOG)"
  grep -iE "error|fail|uncaught" "$LOG" | grep -vi "favicon\|GPU stall\|dbus" | head -15
fi
exit $fail
