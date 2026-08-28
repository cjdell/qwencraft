#!/usr/bin/env bash
# Build the RustCraft wasm web app AND the headless server binary, and deploy
# both to the router.
#
# The web build (web/dist/: index.html + pkg/*.wasm + *.js) is rsynced to
# /srv/rustcraft, where nginx serves the game page under
# rustcraft.home.chrisdell.info. The native `rustcraft-net` binary (built for
# the build box's arch — the router is the same x86_64) is rsynced to
# /srv/rustcraft-server, where the router's NixOS config runs it as a systemd
# service (rustcraft-net.service) on 127.0.0.1:9000; nginx proxies /ws,
# /dashboard, /api and /healthz for the same domain to it. The vhost +
# service live in hosts/grafton-router/services/rustcraft.nix in the router's
# own nixos-config repo. The binary is a glibc build for the build box, so
# the service runs it through the router's dynamic loader (that glue lives
# in the NixOS unit, not here). NOTE: /srv/rustcraft-server must exist and
# be owned by $DEPLOY_USER (one-time bootstrap: sudo mkdir + chown).
#
# Usage:
#   ./deploy.sh                 # deploy to the default host (192.168.49.1)
#   HOST=10.0.0.1 ./deploy.sh   # override target host
#
# Env overrides:
#   DEPLOY_HOST        router IP/hostname        (default 192.168.49.1)
#   DEPLOY_USER        SSH user                  (default cjdell)
#   DEPLOY_SSH_KEY     SSH private key           (default ~/.ssh/id_rsa)
#   DEPLOY_DEST        remote web root           (default /srv/rustcraft)
#   DEPLOY_SERVER_DEST remote server bin dir     (default /srv/rustcraft-server)
#
# The build tools (cargo, wasm-bindgen) live in the Nix dev shell. If this
# script is run outside `nix develop`, it re-execs itself through the shell.
set -euo pipefail

# deploy.sh lives at the repo root (unlike scripts/build.sh, which is one dir
# down and needs the trailing /..), so just cd into the script's own directory.
cd "$(dirname "$0")"

# Root filesystem on the build box is full, so cargo needs a scratch TMPDIR.
export TMPDIR="${TMPDIR:-/home/cjdell/tmp}"
mkdir -p "$TMPDIR"

DEPLOY_HOST="${DEPLOY_HOST:-192.168.49.1}"
DEPLOY_USER="${DEPLOY_USER:-cjdell}"
DEPLOY_SSH_KEY="${DEPLOY_SSH_KEY:-$HOME/.ssh/id_rsa}"
DEPLOY_DEST="${DEPLOY_DEST:-/srv/rustcraft}"
# Separate dir from the web root: the web rsync below uses --delete, so the
# server binary must not live under $DEPLOY_DEST or it would be wiped.
DEPLOY_SERVER_DEST="${DEPLOY_SERVER_DEST:-/srv/rustcraft-server}"
# Cargo crate name (hyphen) vs. produced wasm artifact name (underscore).
CRATE_NAME="rustcraft-web"
WASM_ARTIFACT="rustcraft_web"
SERVER_CRATE="rustcraft-net"

# Always build inside the Nix dev shell (golden rule: everything runs through
# it) so cargo/wasm-bindgen use the pinned toolchain and wasm32 target. The
# marker env var prevents re-exec recursion once we're already inside it.
if [ -z "${_RUSTCRAFT_DEPLOYED:-}" ]; then
  export _RUSTCRAFT_DEPLOYED=1
  exec nix develop --command bash "$0" "$@"
fi

echo "==> Building ($CRATE_NAME, wasm32-unknown-unknown, release)"
cargo build --release --target wasm32-unknown-unknown -p "$CRATE_NAME"

echo "==> Building ($SERVER_CRATE, native, release)"
cargo build --release -p "$SERVER_CRATE"

echo "==> wasm-bindgen -> web/dist"
rm -rf web/dist
mkdir -p web/dist/pkg
wasm-bindgen --target web --out-dir web/dist/pkg \
  target/wasm32-unknown-unknown/release/${WASM_ARTIFACT}.wasm
cp web/index.html web/dist/index.html

echo "==> rsync web/dist/ -> ${DEPLOY_USER}@${DEPLOY_HOST}:${DEPLOY_DEST}"
rsync -avz --delete \
  -e "ssh -i ${DEPLOY_SSH_KEY}" \
  web/dist/ "${DEPLOY_USER}@${DEPLOY_HOST}:${DEPLOY_DEST}/"

# rsync does not create intermediate dirs, so make sure the target exists
# before shipping the binary.
ssh -i "${DEPLOY_SSH_KEY}" \
  "${DEPLOY_USER}@${DEPLOY_HOST}" "mkdir -p '${DEPLOY_SERVER_DEST}'"

# No --delete here: single file, and the remote dir may hold other files.
echo "==> rsync ${SERVER_CRATE} -> ${DEPLOY_USER}@${DEPLOY_HOST}:${DEPLOY_SERVER_DEST}"
rsync -av \
  -e "ssh -i ${DEPLOY_SSH_KEY}" \
  target/release/${SERVER_CRATE} \
  "${DEPLOY_USER}@${DEPLOY_HOST}:${DEPLOY_SERVER_DEST}/${SERVER_CRATE}"

# The service runs this exact file from /srv, so a fresh binary only takes
# effect on a restart. Best-effort: on a first-time deploy the unit may not
# exist yet (it is created by the router's NixOS config + switch).
if ssh -i "${DEPLOY_SSH_KEY}" "${DEPLOY_USER}@${DEPLOY_HOST}" \
    "systemctl list-unit-files rustcraft-net.service | grep -q '^rustcraft-net'"; then
  echo "==> restarting rustcraft-net.service (picks up the new binary)"
  ssh -i "${DEPLOY_SSH_KEY}" "${DEPLOY_USER}@${DEPLOY_HOST}" \
    "sudo -n systemctl restart rustcraft-net"
else
  echo "==> note: rustcraft-net.service not present on ${DEPLOY_HOST} yet —"
  echo "    it is created by the router's NixOS config; start it after the next switch."
fi

echo "==> done: ${DEPLOY_USER}@${DEPLOY_HOST}:${DEPLOY_DEST} + ${DEPLOY_SERVER_DEST} (rustcraft.home.chrisdell.info)"
