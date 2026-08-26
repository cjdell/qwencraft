#!/usr/bin/env bash
# Headless NPC load test.
#
# Spawns COUNT NPCs at SPACING-block spacing inside the browser
# (?npcs=COUNT:SPACING arms the load on the first tick — the in-game
# equivalent is the N / C / I / U / [ / ] keys) and checks:
#   - the app boots and renders with the load active (no JS errors),
#   - the HUD reports the live NPC count,
#   - the local-block-window stats prove steady-state physics runs on the
#     per-agent cache: window hit rate >= 99% and solid fallbacks stay a
#     tiny fraction of lookups (they should be ~0 after the spawn tick).
#
# The real per-tick CPU cost is best measured with the host benchmark:
#   cargo run -p rustcraft-server --release --example bench_tick
# (headless virtual-time mode distorts Date.now(), so browser PERF timing
#  there is not meaningful).
#
# Usage: ./scripts/npc_test.sh [COUNT] [SPACING] [BUDGET_MS]
# Defaults: 500 24 20000
set -uo pipefail
cd "$(dirname "$0")/.."

COUNT="${1:-500}"
SPACING="${2:-24}"
BUDGET="${3:-20000}"
PORT="${PORT:-$((20000 + RANDOM % 20000))}"
export TMPDIR="${TMPDIR:-/tmp}"
LOG="${TMPDIR}/rustcraft-npc.log"
PROF_DIR="${TMPDIR}/rustcraft-npc-prof"

if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi

# Force a lavapipe (software Vulkan) ICD for WebGPU (see verify.sh).
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

rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"

python3 -m http.server "$PORT" --directory web/dist --bind 127.0.0.1 >/dev/null 2>&1 &
SRV=$!
cleanup() { kill "$SRV" 2>/dev/null || true; }
trap cleanup EXIT
sleep 1

curl -sf "http://127.0.0.1:${PORT}/" | grep -q "rustcraft" || {
  echo "FAIL: port ${PORT} is not serving our web/dist (port squatted? rerun)"
  exit 1
}

echo "==> headless chromium: ${COUNT} NPCs @ ${SPACING}m spacing (${BUDGET}ms virtual time)"
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
  --dump-dom "http://127.0.0.1:${PORT}/?seed=1337&npcs=${COUNT}:${SPACING}" \
  >"$LOG" 2>&1 || true

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

check "app started"           grep -q "RustCraft: app started" "$LOG"
check "renderer ready"        grep -q "RustCraft: renderer ready" "$LOG"
check "load armed"            grep -q "NPC load test armed: ${COUNT} agents" "$LOG"
check "no uncaught JS errors" bash -c "! grep -E 'Uncaught|TypeError|ReferenceError' '$LOG' | grep -v 'favicon' | grep -q ."

# The HUD line carries the live count and the window stats.
NPC_LINE=$(grep -o "npc ${COUNT}/${SPACING}m.*window [0-9.]*% · solid-fb [0-9]* · rebuilds [0-9]*" "$LOG" | tail -1)
if [ -z "$NPC_LINE" ]; then
  echo "FAIL: HUD has no npc load line for ${COUNT}/${SPACING}m"
  fail=1
else
  echo "  HUD: $NPC_LINE"
  check "live count matches"    bash -c "grep -q '(live ${COUNT})' <<< '$NPC_LINE'"
  HIT=$(grep -o "window [0-9.]*%" <<<"$NPC_LINE" | grep -o "[0-9.]*")
  SOLID_FB=$(grep -o "solid-fb [0-9]*" <<<"$NPC_LINE" | grep -o "[0-9]*")
  REBUILDS=$(grep -o "rebuilds [0-9]*" <<<"$NPC_LINE" | grep -o "[0-9]*")
  # Window hit rate must be ~100%: steady-state physics is served by the
  # per-agent local block window, not the world's chunk buffers.
  python3 -c "import sys; sys.exit(0 if float('$HIT') >= 99.0 else 1)" \
    && echo "PASS: window hit rate ${HIT}% (>= 99%)" \
    || { echo "FAIL: window hit rate ${HIT}% < 99% — physics is hitting the chunk buffers"; fail=1; }
  # Solid fallbacks: only the spawn tick (before each window's first build)
  # can produce them, so they must stay far below the lookup volume. The
  # HUD shows absolute counts; sanity-check the scale (<= ~2 per NPC).
  python3 -c "import sys; sys.exit(0 if int('$SOLID_FB') <= ${COUNT} * 2 else 1)" \
    && echo "PASS: solid fallbacks $SOLID_FB (transient spawn-tick only, <= ${COUNT}x2)" \
    || { echo "FAIL: solid fallbacks $SOLID_FB — too many steady-state world lookups"; fail=1; }
  echo "  window rebuilds since load: $REBUILDS"
fi

echo
if [ "$fail" = 0 ]; then
  echo "NPC LOAD TEST PASSED (${COUNT} @ ${SPACING}m; log: $LOG)"
else
  echo "NPC LOAD TEST FAILED (log: $LOG)"
  grep -iE "error|uncaught" "$LOG" | grep -vi "favicon\|GPU stall\|dbus" | head -10
fi
exit $fail
