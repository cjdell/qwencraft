#!/usr/bin/env bash
# Headless WALK stress test for the RustCraft web app.
#
# The app runs in ?walk=1 mode: it holds W and slowly turns for ~60 virtual
# seconds, walking through fresh terrain. This is exactly the scenario that
# used to fill the client's terrain buffer pool and permanently drop chunks
# (invisible holes in the landscape; the player still collided with them).
#
# Checks:
#   - the player actually walked far from spawn (HUD position),
#   - frames kept being rendered the whole time (PERF lines),
#   - NO "terrain pool full — chunk lost" warnings (the drop path),
#   - NO "requesting re-send" events (the safety net should not be needed
#     with a correctly sized pool),
#   - reports how many pool compactions happened (informational).
#
# Run inside `nix develop`. Expects ./scripts/build.sh to have been run.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-$((20000 + RANDOM % 20000))}"
TMPDIR="${TMPDIR:-/tmp}"
LOG="${LOG:-${TMPDIR}/rustcraft-walk.log}"
DOM="${DOM:-${TMPDIR}/rustcraft-walk-dom.html}"
BUDGET="${BUDGET:-90000}"

if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi

# Force the lavapipe (software Vulkan) ICD (see verify.sh).
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
export TMPDIR

PROF_DIR="${TMPDIR}/rustcraft-walk-prof"
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

echo "==> headless chromium walk test (${BUDGET}ms virtual time, ${VK_ICD:-auto Vulkan})"
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
  --dump-dom "http://127.0.0.1:${PORT}/?walk=1" >"$DOM" 2>"$LOG" || true

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
check "first frame rendered"       grep -q "RustCraft: first frame rendered" "$LOG"
check "no uncaught JS errors"      bash -c "! grep -E 'Uncaught|TypeError|ReferenceError' '$LOG' | grep -v 'favicon' | grep -q ."

# The pool must not have lost chunks (the old bug).
check "no 'pool full — chunk lost' warnings" bash -c "! grep -q 'chunk lost' '$LOG'"
check "no chunk re-sends needed"        bash -c "! grep -q 're-send' '$LOG'"

# Frames kept being rendered for the whole run (PERF is logged every 20 HUD
# updates, i.e. every ~10s of app time).
NP=$(grep -c "^.*PERF fps=" "$LOG" 2>/dev/null || true)
NP="${NP:-0}"
check "frame loop ran the whole time (PERF lines: $NP)" test "$NP" -ge 3

# The player must actually have walked a substantial path length (the
# walker turns around, so use the accumulated distance, not displacement),
# and the t=30s fly phase must have engaged at near-max speed.
WALK_LINE=$(grep -o '"WALK [^"]*"' "$LOG" | tail -1 | sed 's/"//g')
DIST=$(echo "$WALK_LINE" | grep -o 'dist=[0-9.]*' | cut -d= -f2)
FLY=$(echo "$WALK_LINE" | grep -o 'fly=[01]' | cut -d= -f2)
SPEED=$(echo "$WALK_LINE" | grep -o 'speed=[0-9.]*' | cut -d= -f2)
if [ -z "$DIST" ]; then
  echo "FAIL: no WALK telemetry in log"
  fail=1
else
  python3 - "$DIST" <<'PY'
import sys
d = float(sys.argv[1])
print(f"  player travelled {d:.0f} blocks (path length, walk+fly)")
sys.exit(0 if d >= 120 else 1)
PY
  [ $? -eq 0 ] && echo "PASS: player walked through fresh terrain" || { echo "FAIL: player did not walk far enough ($DIST blocks)"; fail=1; }
fi
check "fly phase engaged (F toggle)" test "${FLY:-0}" = "1"
if [ -n "${SPEED:-}" ]; then
  python3 - "$SPEED" <<'PY' && echo "PASS: fly speed ramped near max" || { echo "FAIL: fly speed only reached $SPEED b/s"; fail=1; }
import sys
s = float(sys.argv[1])
sys.exit(0 if s >= 400 else 1)
PY
fi

# Informational: walk steering + pool activity.
TURNS=$(grep -c "stuck at" "$LOG" 2>/dev/null || true)
echo "  walk steering turns: ${TURNS:-0}"
COMPACT=$(grep -c "compacted terrain pool" "$LOG" 2>/dev/null || true)
echo "  pool compactions: ${COMPACT:-0}"
if [ "${COMPACT:-0}" -gt 0 ]; then
  grep "compacted terrain pool" "$LOG" | tail -3 | sed 's/.*CONSOLE[^"]*//' | sed 's/^ *//'
fi
CHUNKS=$(grep -o "chunks [0-9]* sent / [0-9]* gen" "$DOM" | tail -1)
echo "  final HUD: ${CHUNKS:-n/a}"

echo
if [ "$fail" = 0 ]; then
  echo "WALK TEST PASSED (log: $LOG, dom: $DOM)"
else
  echo "WALK TEST FAILED (log: $LOG)"
  grep -iE "error|fail|chunk lost|pool" "$LOG" | grep -vi "favicon\|dbus" | head -15
fi
exit $fail
