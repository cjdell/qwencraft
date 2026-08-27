#!/usr/bin/env bash
# Headless-browser test for the SERVER DASHBOARD: rustcraft-net's built-in
# HTTP server serves the embedded dioxus dashboard, which polls
# /api/status + /api/map and renders a 2D minimap.
#
# - builds the rustcraft-net binary (release) if missing,
# - starts it with a random WebSocket + HTTP port,
# - curl-checks the HTTP endpoints (health, status JSON, map binary, assets),
# - runs headless Chromium on the dashboard page and checks the DOM shows a
#   live server (seed, players count, startup event) and the screenshot
#   shows the rendered minimap (not a blank pane).
#
# Run inside `nix develop`.
set -uo pipefail
cd "$(dirname "$0")/.."

WS_PORT="${WS_PORT:-$((40000 + RANDOM % 20000))}"
HTTP_PORT="${HTTP_PORT:-$((20000 + RANDOM % 20000))}"
SEED="${SEED:-1337}"
LOG="${LOG:-${TMPDIR:-/tmp}/rustcraft-dashboard-chrome.log}"
SHOT="${SHOT:-${TMPDIR:-/tmp}/rustcraft-dashboard-shot.png}"
DOM="${DOM:-${TMPDIR:-/tmp}/rustcraft-dashboard-dom.html}"
SRV_LOG="${SRV_LOG:-${TMPDIR:-/tmp}/rustcraft-dashboard-server.log}"
BUDGET="${BUDGET:-20000}"

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
PROF_DIR="${TMPDIR}/rustcraft-dashboard-chrome-prof"
rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"

echo "==> headless server (ws://127.0.0.1:${WS_PORT}, http://127.0.0.1:${HTTP_PORT}, seed ${SEED})"
"$SRV_BIN" --seed "$SEED" --port "$WS_PORT" --bind 127.0.0.1 \
  --http-port "$HTTP_PORT" >"$SRV_LOG" 2>&1 &
SRV=$!
cleanup() { kill "$SRV" 2>/dev/null || true; }
trap cleanup EXIT

for _ in $(seq 1 50); do
  grep -q "rustcraft-net: ready" "$SRV_LOG" 2>/dev/null && break
  sleep 0.2
done
if ! grep -q "rustcraft-net: ready" "$SRV_LOG" 2>/dev/null; then
  echo "FAIL: headless server did not start" >&2
  tail -5 "$SRV_LOG" >&2
  exit 1
fi

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

BASE="http://127.0.0.1:${HTTP_PORT}"

# --- HTTP endpoint checks (curl) -------------------------------------------
check "health probe"        bash -c "curl -sf '$BASE/healthz' | grep -q ok"
check "status: seed"        bash -c "curl -sf '$BASE/api/status' | grep -q '\"seed\":${SEED}'"
check "status: zero players" bash -c "curl -sf '$BASE/api/status' | grep -q '\"players\":0'"
check "status: startup event" bash -c "curl -sf '$BASE/api/status' | grep -q 'server started'"
check "status: content-type json" \
  bash -c "curl -s -o /dev/null -D - '$BASE/api/status' | grep -qi 'Content-Type: application/json'"
check "map: 64x64 is 8192 bytes" \
  bash -c "[ \$(curl -sf '$BASE/api/map?x=8&z=8&w=64&h=64' | wc -c) -eq 8192 ]"
check "map: oversized clamps to 256" \
  bash -c "[ \$(curl -sf '$BASE/api/map?x=8&z=8&w=4096&h=4096' | wc -c) -eq \$((256*256*2)) ]"
check "dashboard page"      bash -c "curl -sf '$BASE/' | grep -q 'RustCraft server'"
check "dashboard js asset"  bash -c "curl -sf '$BASE/rustcraft_dashboard.js' | grep -q rustcraft"
check "dashboard wasm asset" \
  bash -c "[ \$(curl -sf '$BASE/rustcraft_dashboard_bg.wasm' | wc -c) -gt 10000 ]"
check "dashboard css asset" \
  bash -c "curl -s -o /dev/null -D - '$BASE/dashboard.css' | grep -qi 'Content-Type: text/css'"
check "unknown path 404s"   bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' '$BASE/nope') -eq 404 ]"

# --- Headless Chromium on the dashboard page --------------------------------
echo "==> headless chromium on the dashboard (virtual time ${BUDGET}ms)"
chromium \
  --headless \
  --no-sandbox \
  --disable-gpu-sandbox \
  --enable-unsafe-webgpu \
  --enable-unsafe-swiftshader \
  --use-angle=swiftshader \
  --user-data-dir="$PROF_DIR" \
  --window-size=1280,720 \
  --enable-logging=stderr --v=0 \
  --virtual-time-budget="$BUDGET" \
  --screenshot="$SHOT" \
  "$BASE/" \
  >"$LOG" 2>&1 || true

check "no uncaught JS errors" \
  bash -c "! grep -E 'Uncaught|TypeError|ReferenceError' '$LOG' | grep -v 'favicon' | grep -q ."
check "no dashboard init failure" \
  bash -c "! grep -q 'dashboard init failed\|dashboard failed to start' '$LOG'"

# DOM: the app mounted and shows the live server.
chromium --headless --no-sandbox --enable-unsafe-webgpu --use-angle=swiftshader \
  --enable-unsafe-swiftshader \
  --user-data-dir="$PROF_DIR" \
  --window-size=1280,720 --virtual-time-budget="$BUDGET" \
  --dump-dom "$BASE/" >"$DOM" 2>/dev/null || true

check "app title rendered"        grep -q "RustCraft server" "$DOM"
check "seed shown in the top bar" grep -q "${SEED} seed" "$DOM"
check "players stat rendered"     grep -q "players connected" "$DOM"
check "startup event in the log"  grep -q "server started" "$DOM"
check "map canvas present"        grep -q 'id="map-canvas"' "$DOM"
check "not stuck on the init fallback" \
  bash -c "! grep -q '<pre style=.color:#f66' '$DOM'"

# Screenshot: the minimap must have real content (coloured terrain), not a
# flat dark pane.
check "screenshot created" test -s "$SHOT"
if python3 - "$SHOT" <<'PY'
import struct, sys, zlib

def png_pixels(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a png"
    pos = 8
    width = height = bitdepth = colortype = None
    idat = b""
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos+4])
        ctype = data[pos+4:pos+8]
        chunk = data[pos+8:pos+8+length]
        if ctype == b"IHDR":
            width, height, bitdepth, colortype = struct.unpack(">IIBB", chunk[:10])
        elif ctype == b"IDAT":
            idat += chunk
        pos += 12 + length
    assert bitdepth == 8 and colortype in (2, 6), f"unsupported png (depth={bitdepth}, type={colortype})"
    channels = 4 if colortype == 6 else 3
    raw = zlib.decompress(idat)
    stride = width * channels
    out = bytearray()
    prev = bytearray(stride)
    p = 0
    for _ in range(height):
        f = raw[p]; p += 1
        line = bytearray(raw[p:p+stride]); p += stride
        for i in range(stride):
            a = line[i - channels] if i >= channels else 0
            b = prev[i]
            c = prev[i - channels] if i >= channels else 0
            if f == 0: pass
            elif f == 1: line[i] = (line[i] + a) & 0xFF
            elif f == 2: line[i] = (line[i] + b) & 0xFF
            elif f == 3: line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif f == 4:
                pp = a + b - c
                pa = abs(pp - a)
                pb = abs(pp - b)
                pc = abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out += line
        prev = line
    return width, height, channels, bytes(out)

w, h, ch, px = png_pixels(sys.argv[1])
samples = px[::8*ch]
distinct = len(set(samples))
# The page is a dark UI; a rendered map adds many distinct colours (grass /
# water / sand / stone / tree greens). A blank pane stays nearly uniform.
print(f"  screenshot {w}x{h}, distinct sampled colours: {distinct}")
if distinct < 25:
    print("FAIL: dashboard looks blank (single-colour screen)")
    sys.exit(1)
print("PASS: screenshot has rendered content")
PY
then
  :
else
  echo "FAIL: dashboard screenshot blank or missing"
  fail=1
fi

echo
if [ "$fail" = 0 ]; then
  echo "ALL CHECKS PASSED (shot: $SHOT, dom: $DOM, log: $LOG, server: $SRV_LOG)"
else
  echo "SOME CHECKS FAILED (log: $LOG, server: $SRV_LOG)"
  grep -iE "error|fail|uncaught" "$LOG" | grep -vi "favicon\|GPU stall\|dbus" | head -15
fi
exit $fail
