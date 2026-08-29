#!/usr/bin/env bash
# Headless-browser test for the MOBILE touch controls (two thumb pads).
#
# - serves web/dist, runs headless Chromium in a PHONE-sized window
#   (390x844 portrait) with ?touchtest=1 — that forces touch mode even on
#   a fine-pointer headless browser and injects a self-test script that
#   drives the pads with real TouchEvents (tap-to-play, look drag,
#   joystick walk, JUMP hold, BREAK/PLACE on the crosshair, hotbar tap),
# - asserts each TOUCHTEST phase succeeded: the overlay starts on a tap,
#   the look pad turns the camera, the joystick walks the player at
#   stick-throttle speed, JUMP leaves the ground, BREAK/PLACE edit the
#   world through the authoritative server (verified with qwc.getBlock),
#   and the tapped hotbar slot is the block that gets placed,
# - real time (no --virtual-time-budget): the built-in server ticks at
#   wall-clock 60 Hz and a cold SwiftShader WebGPU init can take ~20 s.
#
# Run inside `nix develop`. Expects ./scripts/build.sh to have been run.
set -uo pipefail
cd "$(dirname "$0")/.."

# Use a random high port: low ports (8080/8090/...) may be squatted by
# unrelated host processes.
PORT="${PORT:-$((20000 + RANDOM % 20000))}"
SEED="${SEED:-1337}"
LOG="${LOG:-${TMPDIR:-/tmp}/qwencraft-touch-chrome.log}"
# Wall-clock seconds for the page to boot (cold SwiftShader init ~20 s) and
# run the scripted touch sequence (~15 s).
RUN_SECS="${RUN_SECS:-75}"

if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
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

# Chromium needs scratch space (profile, GPU temp files).
export TMPDIR="${TMPDIR:-/tmp}"
PROF_DIR="${TMPDIR}/qwencraft-touch-chrome-prof"
rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"

python3 -m http.server "$PORT" --directory web/dist --bind 127.0.0.1 >/dev/null 2>&1 &
SRV=$!
cleanup() { kill "$SRV" 2>/dev/null || true; }
trap cleanup EXIT

echo "==> headless chromium (phone window 390x844, ?touchtest=1, ${RUN_SECS}s real time)"
timeout "$RUN_SECS" chromium \
  --headless \
  --no-sandbox \
  --disable-gpu-sandbox \
  --enable-unsafe-webgpu \
  --enable-unsafe-swiftshader \
  --use-angle=swiftshader \
  --user-data-dir="$PROF_DIR" \
  --window-size=390,844 \
  --enable-logging=stderr --v=0 \
  "http://127.0.0.1:${PORT}/?seed=${SEED}&touchtest=1" \
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

check "app started"               grep -q "Qwencraft: app started" "$LOG"
check "touch mode detected"       grep -q "Qwencraft: touch controls enabled" "$LOG"
check "tap-to-play started game"  grep -q "Qwencraft: touch controls active (tap-to-play)" "$LOG"
check "start: overlay dismissed"  grep -q "TOUCHTEST start ok" "$LOG"
check "look pad turns camera"     grep -q "TOUCHTEST look ok" "$LOG"
check "move pad walks player"     grep -q "TOUCHTEST move ok" "$LOG"
check "jump button works"         grep -q "TOUCHTEST jump ok" "$LOG"
check "break button breaks block" grep -q "TOUCHTEST break ok" "$LOG"
check "place button places block" grep -q "TOUCHTEST place ok" "$LOG"
check "hotbar tap selects block"  grep -q "TOUCHTEST hotbar ok" "$LOG"
check "self-test completed"       grep -q "TOUCHTEST ALL OK" "$LOG"
check "no self-test failure"      bash -c "! grep -q 'TOUCHTEST FAIL' '$LOG'"
check "no uncaught JS errors"     bash -c "! grep -E 'Uncaught|TypeError|ReferenceError' '$LOG' | grep -v 'favicon' | grep -q ."

echo
if [ "$fail" = 0 ]; then
  echo "TOUCH TEST PASSED (log: $LOG)"
else
  echo "TOUCH TEST FAILED (log: $LOG)"
  grep -E "TOUCHTEST|Uncaught|TypeError|ReferenceError" "$LOG" | grep -v favicon | head -15
fi
exit $fail
