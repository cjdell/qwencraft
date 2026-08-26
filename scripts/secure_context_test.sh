#!/usr/bin/env bash
# WebGPU secure-context regression test.
#
# Browsers only expose WebGPU in secure contexts (https:// or localhost):
#   A. http://<LAN-IP>   -> the app must fail GRACEFULLY (friendly overlay
#                           message, no wasm panic)
#   B. https://127.0.0.1 -> full startup (self-signed cert)
#   C. https://<LAN-IP>  -> full startup (the "play from another device" case)
#
# A and C are skipped when the machine has no non-loopback IPv4 address.
set -uo pipefail
cd "$(dirname "$0")/.."
if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi

# Force lavapipe for WebGPU (same logic as verify.sh).
VK_ICD=""
for f in /usr/share/vulkan/icd.d/*.json /etc/vulkan/icd.d/*.json; do
  if [ -f "$f" ] && grep -qi "lvp\|swrast" "$f" 2>/dev/null; then
    VK_ICD="$f"
    break
  fi
done
export VK_ICD_FILENAMES="${VK_ICD}"

export TMPDIR="${TMPDIR:-/tmp}"
PROF_DIR="${TMPDIR}/rustcraft-ctx-prof"
rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"

LAN_IP="$(ip -4 -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | grep -v '^127\.' | head -1)"
echo "LAN IP: ${LAN_IP:-<none — parts A and C will be skipped>}"

PORT_H=8191
PORT_S=8192
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

chrome_dump() { # url log dom
  chromium \
    --headless \
    --no-sandbox \
    --disable-gpu-sandbox \
    --enable-unsafe-webgpu \
    --enable-unsafe-swiftshader \
    --use-angle=swiftshader \
    --ignore-certificate-errors \
    --user-data-dir="$PROF_DIR" \
    --window-size=1280,720 \
    --enable-logging=stderr --v=0 \
    --virtual-time-budget=20000 \
    --dump-dom "$1" >"$3" 2>"$2" || true
}

SRV=""
SRV2=""
cleanup() {
  [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
  [ -n "$SRV2" ] && kill "$SRV2" 2>/dev/null
}
trap cleanup EXIT

# ---- A: plain HTTP on a LAN address -> graceful failure -------------------
if [ -n "$LAN_IP" ]; then
  echo "===> A: http://${LAN_IP}:${PORT_H} (expect graceful WebGPU error)"
  python3 -m http.server "$PORT_H" --directory web/dist --bind 0.0.0.0 >/dev/null 2>&1 &
  SRV=$!
  sleep 1
  chrome_dump "http://${LAN_IP}:${PORT_H}/?seed=1337" "$PROF_DIR/a.log" "$PROF_DIR/a.dom"
  check "no wasm panic over plain HTTP" \
    bash -c "! grep -Eq 'Uncaught|rust_panic' '$PROF_DIR/a.log'"
  check "friendly 'WebGPU is unavailable' message shown" \
    grep -q "WebGPU is unavailable" "$PROF_DIR/a.dom"
  kill "$SRV" 2>/dev/null; SRV=""
else
  echo "SKIP: A (no LAN IP)"
fi

# ---- B + C: HTTPS with the self-signed cert -------------------------------
echo "===> serving HTTPS on :${PORT_S} (./scripts/serve.sh --https)"
PORT="$PORT_S" bash scripts/serve.sh --https >"$PROF_DIR/serve.log" 2>&1 &
SRV2=$!
# Cert generation + server startup.
for _ in $(seq 1 30); do
  python3 - "$PORT_S" <<'PY' && break
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.3)
s.close()
PY
  sleep 0.5
done

echo "===> B: https://127.0.0.1:${PORT_S} (expect full startup)"
chrome_dump "https://127.0.0.1:${PORT_S}/?seed=1337" "$PROF_DIR/b.log" "$PROF_DIR/b.dom"
check "https localhost: app started" grep -q "RustCraft: app started" "$PROF_DIR/b.log"
check "https localhost: renderer ready (WebGPU)" grep -q "RustCraft: renderer ready" "$PROF_DIR/b.log"
check "https localhost: first frame rendered" grep -q "RustCraft: first frame rendered" "$PROF_DIR/b.log"

if [ -n "$LAN_IP" ]; then
  echo "===> C: https://${LAN_IP}:${PORT_S} (expect full startup — the LAN case)"
  chrome_dump "https://${LAN_IP}:${PORT_S}/?seed=1337" "$PROF_DIR/c.log" "$PROF_DIR/c.dom"
  check "https LAN: app started" grep -q "RustCraft: app started" "$PROF_DIR/c.log"
  check "https LAN: renderer ready (WebGPU)" grep -q "RustCraft: renderer ready" "$PROF_DIR/c.log"
else
  echo "SKIP: C (no LAN IP)"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "SECURE-CONTEXT TEST PASSED (artifacts: $PROF_DIR)"
else
  echo "SOME CHECKS FAILED (artifacts: $PROF_DIR)"
  exit 1
fi
