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
# World seed. The pool-capacity regression lives on dense-terrain seeds
# (e.g. SEED=888 or 31337, the worst views per pool_measure) — an
# under-sized pool only loses chunks once the view fills it.
SEED="${SEED:-1337}"

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
  --dump-dom "http://127.0.0.1:${PORT}/?walk=1&seed=${SEED}" >"$DOM" 2>"$LOG" || true

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
check "return leg ran"                  grep -q "turn around" "$LOG"
# Terrain pool integrity: the POOL line reports chunks held + meshed-but-
# evicted chunks that are clearly visible (holes). The run ends with the
# player walking back through terrain evicted on the way out, so a broken
# eviction->re-stream path (or an under-sized pool) would leave hundreds of
# visible chunks missing. Assert the MAX over the whole run, not just the
# final sample: a pool that thrashes (visible chunks evicted and re-queued)
# can show holes at intermediate moments even if the final view recovered.
POOL_LINES=$(grep -o "POOL chunks=[0-9]* missing=[0-9]*" "$LOG")
if [ -z "$POOL_LINES" ]; then
  echo "FAIL: no POOL telemetry in log"
  fail=1
else
  POOL_LINE=$(echo "$POOL_LINES" | tail -1)
  MISSING_LIST=$(echo "$POOL_LINES" | grep -o 'missing=[0-9]*' | cut -d= -f2 | tr '\n' ' ')
  # The re-stream refills evicted visible chunks within a couple of app
  # seconds, far shorter than the ~10s POOL sample interval, so a healthy
  # run only ever shows a single-sample transient (e.g. re-entering fast
  # after the fly phase). The bug this guards against — the user's "chunks
  # don't render until I edit a block" — is SUSTAINED: a broken
  # eviction->re-stream path leaves the re-entered terrain missing for the
  # rest of the run (measured: ~27+ on sparse seeds, ~200+ on dense, across
  # 3+ consecutive samples). So fail on a run of 3+ consecutive samples
  # above 15, or a final sample above 15 — not on single transient spikes.
  # (Healthy walk-back missing is ~0-10; the 15 threshold sits in the gap.)
  python3 - $MISSING_LIST <<'PY' && echo "PASS: no sustained visible holes (missing series: $MISSING_LIST)" || { echo "FAIL: sustained visible holes (missing series: $MISSING_LIST)"; fail=1; }
import sys
vals = [int(v) for v in sys.argv[1:]]
sustained = any(all(v > 15 for v in vals[i:i + 3]) for i in range(len(vals) - 2))
sys.exit(1 if (sustained or vals[-1] > 15) else 0)
PY
fi

# Frames kept being rendered for the whole run (PERF is logged every 20 HUD
# updates, i.e. every ~10s of app time).
NP=$(grep -c "^.*PERF fps=" "$LOG" 2>/dev/null || true)
NP="${NP:-0}"
check "frame loop ran the whole time (PERF lines: $NP)" test "$NP" -ge 3

# The player must actually have walked a substantial path length (the
# walker turns around, so use the accumulated distance, not displacement),
# and the fly phase must have engaged at near-max speed (checked on the
# t=35s telemetry line — during the dive back: the run ends on foot,
# walking back through the re-entered terrain — see the POOL check above).
WALK_LAST=$(grep -o '"WALK t=[0-9]*s pos=[^"]*"' "$LOG" | tail -1 | sed 's/"//g')
WALK_FLY=$(grep -o '"WALK t=35s[^"]*fly=[01][^"]*"' "$LOG" | tail -1 | sed 's/"//g')
DIST=$(echo "$WALK_LAST" | grep -o 'dist=[0-9.]*' | cut -d= -f2)
FLY=$(echo "$WALK_FLY" | grep -o 'fly=[01]' | cut -d= -f2)
SPEED=$(echo "$WALK_FLY" | grep -o 'speed=[0-9.]*' | cut -d= -f2)
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
