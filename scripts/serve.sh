#!/usr/bin/env bash
# Serve web/dist on http://localhost:8080 (override with PORT=...).
#
#   ./scripts/serve.sh           plain HTTP (WebGPU works on localhost)
#   ./scripts/serve.sh --https   HTTPS with a self-signed cert — required
#                                when opening the app from another device
#                                (browsers only enable WebGPU in secure
#                                contexts: https:// or localhost).
set -euo pipefail
cd "$(dirname "$0")/.."
PORT="${PORT:-8080}"
if [ ! -f web/dist/index.html ]; then
  echo "web/dist not found — run ./scripts/build.sh first" >&2
  exit 1
fi

if [ "${1:-}" != "--https" ]; then
  echo "Serving Qwencraft on http://localhost:${PORT}  (seed via ?seed=123)"
  echo "Other devices on your network need HTTPS (WebGPU is a secure-context"
  echo "feature) — run: ./scripts/serve.sh --https"
  exec python3 -m http.server "$PORT" --directory web/dist --bind 0.0.0.0
fi

# ---- HTTPS mode ----------------------------------------------------------
mkdir -p .certs
if [ ! -f .certs/cert.pem ] || [ ! -f .certs/key.pem ]; then
  echo "Generating self-signed certificate in .certs/ ..."
  openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 825 \
    -keyout .certs/key.pem -out .certs/cert.pem \
    -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1
fi
echo "Serving Qwencraft on https://localhost:${PORT}  (self-signed cert)"
echo "From another device: open https://<this-machine's-LAN-IP>:${PORT}"
echo "The browser will warn about the certificate — click Advanced /"
echo "\"Proceed to localhost (unsafe)\" to continue."
exec python3 - "$PORT" .certs/cert.pem .certs/key.pem <<'PY'
import http.server, socketserver, ssl, sys

port = int(sys.argv[1])
cert, key = sys.argv[2], sys.argv[3]

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory="web/dist", **kwargs)

socketserver.ThreadingTCPServer.allow_reuse_address = True
httpd = socketserver.ThreadingTCPServer(("", port), Handler)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
httpd.serve_forever()
PY
