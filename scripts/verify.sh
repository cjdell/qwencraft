#!/usr/bin/env bash
# Headless-browser smoke test for the RustCraft web app.
#
# - serves web/dist, runs headless Chromium with WebGPU (SwiftShader Vulkan,
#   falling back to lavapipe), fast-forwards ~25s of virtual time,
# - checks the JS console log for the app's startup milestones,
# - takes a screenshot and verifies it contains non-trivial rendered content
#   (pixel variance),
# - dumps the DOM and checks the HUD shows streamed chunks.
#
# Run inside `nix develop`. Expects ./scripts/build.sh to have been run.
set -uo pipefail
cd "$(dirname "$0")/.."

# Use a random high port: low ports (8080/8090/...) may be squatted by
# unrelated host processes.
PORT="${PORT:-$((20000 + RANDOM % 20000))}"
# Default outputs go under $TMPDIR: on machines where the root filesystem
# (and /tmp) is full, callers pass TMPDIR=/some/other/path and everything
# (chromium profile, log, screenshots) follows.
SHOT="${SHOT:-${TMPDIR:-/tmp}/rustcraft-shot.png}"
LOG="${LOG:-${TMPDIR:-/tmp}/rustcraft-chrome.log}"
DOM="${DOM:-${TMPDIR:-/tmp}/rustcraft-dom.html}"
# 40s virtual budget: a COLD SwiftShader device init can consume ~20s of
# virtual time while the 16ms interval fast-forwards (the first run after a
# while); 25s left no budget for the frame-410 pixel readback.
BUDGET="${BUDGET:-40000}"
# The app streams the WebGL2 shadow-rendered scene as base64 VERIFY_PNG
# chunks; we reconstruct a real PNG screenshot of the 3D view here.
SCENE_PNG="${SCENE_PNG:-${TMPDIR:-/tmp}/rustcraft-scene.png}"

if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi

# Find a lavapipe (software Vulkan) ICD. We always force it via
# VK_ICD_FILENAMES: on machines with a (possibly broken) host Vulkan driver,
# letting the browser auto-pick can hang requestAdapter().
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

# Chromium needs scratch space (profile, GPU temp files). Honour a
# pre-set TMPDIR (e.g. when / is full) and give it an explicit profile dir.
export TMPDIR="${TMPDIR:-/tmp}"
PROF_DIR="${TMPDIR}/rustcraft-chrome-prof"
rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"

python3 -m http.server "$PORT" --directory web/dist --bind 127.0.0.1 >/dev/null 2>&1 &
SRV=$!
cleanup() { kill "$SRV" 2>/dev/null || true; }
trap cleanup EXIT
sleep 1

# Sanity: make sure WE are the server on this port.
curl -sf "http://127.0.0.1:${PORT}/" | grep -q "rustcraft" || {
  echo "FAIL: port ${PORT} is not serving our web/dist (port squatted? rerun)"
  exit 1
}

run_chrome() {
  local extra="$1"
  chromium \
    --headless \
    --no-sandbox \
    --disable-gpu-sandbox \
    --enable-unsafe-webgpu \
    --enable-unsafe-swiftshader \
    $extra \
    --user-data-dir="$PROF_DIR" \
    --window-size=1280,720 \
    --enable-logging=stderr --v=0 \
    --virtual-time-budget="$BUDGET" \
    --screenshot="$SHOT" \
    "http://127.0.0.1:${PORT}/?seed=1337&verify=1" \
    >"$LOG" 2>&1 || true
}

echo "==> headless chromium (SwiftShader GL, ${VK_ICD:-auto-picked} Vulkan for WebGPU)"
run_chrome "--use-angle=swiftshader"

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

check "app started"                grep -q "RustCraft: app started" "$LOG"
check "renderer ready (WebGPU)"    grep -q "RustCraft: renderer ready" "$LOG"
check "first frame rendered"       grep -q "RustCraft: first frame rendered" "$LOG"
check "no uncaught JS errors"      bash -c "! grep -E 'Uncaught|TypeError|ReferenceError' '$LOG' | grep -v 'favicon' | grep -q ."

# Screenshot exists and is not a single-colour screen.
check "screenshot created" test -s "$SHOT"
python3 - "$SHOT" <<'PY'
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
                pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out += line
        prev = line
    return width, height, channels, bytes(out)

path = sys.argv[1]
w, h, ch, px = png_pixels(path)
# Sample every 8th pixel for speed.
samples = px[::8*ch]
vals = [b for b in samples[::ch]]  # red channel samples
mean = sum(vals) / len(vals)
var = sum((v - mean) ** 2 for v in vals) / len(vals)
distinct = len(set(samples))
print(f"  screenshot {w}x{h}, distinct sampled colours: {distinct}, red variance: {var:.0f}")
if distinct < 20:
    print("FAIL: screenshot looks like a flat screen (single colour)")
    sys.exit(1)
print("PASS: screenshot has rendered content")
PY
[ $? -eq 0 ] || fail=1

# GPU readback: the app logs a 4x3 grid of region averages (VERIFY_PIXELS)
# so we can verify actual rendered output, not just the CSS background.
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
# Top row: mostly sky (blue dominates red, reasonably bright). A region or
# two may be terrain/trees on the horizon.
top = regs[0:4]
sky = sum(1 for (r, g, b) in top if b > r + 10 and r > 60)
assert sky >= 2, f"top row should be mostly sky: {top}"
# Bottom row: terrain. Greenish (grass) or bluish (water); the pure sky
# clear colour is exactly (135,199,235), so real water is distinguishable.
bottom = regs[8:12]
green = sum(1 for (r, g, b) in bottom if g >= r and g > b + 5)
water = sum(1 for (r, g, b) in bottom if b > g > r and r < 125 and b > 100)
assert green + water >= 2, f"no terrain (grass or water) regions in bottom row: {bottom}"
# Not a flat screen: distinct region colours.
distinct = len(set(regs))
assert distinct >= 4, f"scene looks flat, only {distinct} distinct region colours"
print(f"  GPU readback: 12 regions, top={top[0]} bottom={bottom[0]}, distinct={distinct}")
print("PASS: GPU pixel readback shows sky + terrain")
PY
  [ $? -eq 0 ] || fail=1
fi

# Reconstruct the scene screenshot from the VERIFY_PNG chunks.
if python3 - "$LOG" "$SCENE_PNG" <<'PY'
import base64, struct, sys, zlib
log_path, out_path = sys.argv[1], sys.argv[2]
W, H = 256, 144
chunks = []
with open(log_path, errors="replace") as f:
    for line in f:
        if "VERIFY_PNG" not in line:
            continue
        # Line looks like: ... "VERIFY_PNG 3/17 <b64>", source: ...
        start = line.index("VERIFY_PNG")
        rest = line[start + 10:].strip().rstrip(",")
        toks = rest.split(" ", 1)
        if len(toks) < 2:
            continue
        try:
            idx = int(toks[0].split("/")[0])
        except ValueError:
            continue
        # toks[1] = "<b64>", source: ..." — cut at the closing quote.
        b64 = toks[1].split('"')[0]
        chunks.append((idx, b64))
if len(chunks) < 5:
    print(f"  scene png: only {len(chunks)} chunks in log")
    sys.exit(1)
chunks.sort()
data = b"".join(base64.b64decode(c[1]) for c in chunks)
need = W * H * 4
if len(data) < need:
    print(f"  scene png: decoded {len(data)} bytes, need {need}")
    sys.exit(1)
data = data[:need]
# GL row 0 is the bottom of the screen; PNG rows run top-down.
# The framebuffer is RGBA, so drop the alpha per pixel.
raw = bytearray()
for y in range(H - 1, -1, -1):
    raw.append(0)  # filter: none
    off = y * W * 4
    row = data[off : off + W * 4]
    raw.extend(b for i in range(0, W * 4, 4) for b in row[i : i + 3])

def chunk(tag, payload):
    return (
        struct.pack(">I", len(payload))
        + tag
        + payload
        + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
    )

png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
png += chunk(b"IEND", b"")
with open(out_path, "wb") as f:
    f.write(png)
print(f"  scene screenshot: {W}x{H} PNG at {out_path} ({len(png)} bytes)")
PY
then
  echo "PASS: scene screenshot reconstructed (real rendered 3D view)"
else
  echo "FAIL: could not reconstruct scene screenshot from VERIFY_PNG chunks"
  fail=1
fi

# HUD in the DOM should show streamed chunks (server streaming works).
chromium --headless --no-sandbox --enable-unsafe-webgpu --use-angle=swiftshader --enable-unsafe-swiftshader \
  --user-data-dir="$PROF_DIR" \
  --window-size=1280,720 --virtual-time-budget="$BUDGET" \
  --dump-dom "http://127.0.0.1:${PORT}/?seed=1337" >"$DOM" 2>/dev/null || true
if grep -o "chunks [0-9]* sent" "$DOM" | grep -qv "chunks 0 sent"; then
  echo "PASS: HUD shows streamed chunks ($(grep -o 'chunks [0-9]* sent / [0-9]* gen' "$DOM" | head -1))"
else
  echo "FAIL: HUD shows no streamed chunks"
  fail=1
fi

echo
if [ "$fail" = 0 ]; then
  echo "ALL CHECKS PASSED (scene: $SCENE_PNG, page screenshot: $SHOT, log: $LOG)"
else
  echo "SOME CHECKS FAILED (log: $LOG)"
  grep -iE "error|fail|uncaught" "$LOG" | grep -vi "favicon\|GPU stall\|dbus" | head -15
fi
exit $fail
