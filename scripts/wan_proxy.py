#!/usr/bin/env python3
"""TLS-terminating WAN emulator (threaded, NO asyncio).

Topology replica of the internet deployment:
    browser --wss--> THIS PROXY (TLS end, RTT + rate) --ws--> qwencraft-net

Each connection is owned by ONE thread that services both directions with
select() + blocking send/recv: a full socket simply blocks the sending
thread (that IS the backpressure we want), and a burst is bounded in time
(MAX_BURST_S) — a 60 Hz game server ticks every 16 ms, so an "extend the
burst while data arrives" window with no time cap never closes and nothing
gets forwarded.
  downlink (server->client): WS-frame aware rolling buffer; can drop whole
    frames (env DROP_AFTER_BYTES / DROP_BYTES) — a real network drops
    packets/segments, never a partial frame.
  uplink   (client->server): raw bytes (client->server WS frames are
    masked; no frame semantics needed for the loss model).

Per-direction pump cycle: select for a burst window (collect while data
arrives within COLLECT_S), sleep RTT, sendall (blocking = natural
backpressure), optional rate limit.
"""
import os
import select
import socket
import ssl
import sys
import threading
import time

listen_port = int(sys.argv[1])
cert = sys.argv[2]
key = sys.argv[3]
target_host = sys.argv[4]
target_port = int(sys.argv[5])
rtt_ms = float(sys.argv[6]) if len(sys.argv) > 6 else 150.0
down_mbps = float(sys.argv[7]) if len(sys.argv) > 7 else 0.0
up_mbps = float(sys.argv[8]) if len(sys.argv) > 8 else 0.0
COLLECT_S = 0.020

DROP_AFTER = float(os.environ.get("DROP_AFTER_BYTES", "0"))
DROP_BYTES = float(os.environ.get("DROP_BYTES", "0"))

stats_lock = threading.Lock()
stats = {}


def log(msg):
    with stats_lock:
        print(msg, flush=True)


def ws_frame_end(buf: bytes):
    """Length of the complete unmasked WS frame at the start of buf, or
    None if the buffer holds only a partial frame (or a masked one)."""
    if len(buf) < 2:
        return None
    b1 = buf[1]
    if b1 & 0x80:  # masked — not expected server->client
        return None
    ln = b1 & 0x7F
    hdr = 2
    if ln == 126:
        if len(buf) < 4:
            return None
        ln = int.from_bytes(buf[2:4], "big")
        hdr = 4
    elif ln == 127:
        if len(buf) < 10:
            return None
        ln = int.from_bytes(buf[2:10], "big")
        hdr = 10
    end = hdr + ln
    return end if len(buf) >= end else None


# A burst must be bounded in time: a 60 Hz game server sends tick messages
# every 16 ms, so an "extend while data arrives within window_s" loop would
# NEVER close and the handler would never send. Cap the burst at MAX_BURST_S.
MAX_BURST_S = 0.250


def collect_burst(sources, window_s=COLLECT_S, first_timeout=2.0):
    """Collect bytes from a set of sockets: extend the burst while data keeps
    arriving within window_s, for at most MAX_BURST_S. Returns
    ({sock: bytes}, eof_set). A socket lands in eof_set when recv() returns
    b""."""
    out = {s: b"" for s in sources}
    eof = set()
    pending = set(sources)
    got_any = False
    burst_start = time.monotonic()
    while pending:
        wait = first_timeout if not got_any else window_s
        if got_any and time.monotonic() - burst_start > MAX_BURST_S:
            break
        r, _, _ = select.select(list(pending), [], [], wait)
        if not r:
            if not got_any:
                log(f"[collect] first byte took >{first_timeout}s")
            break
        for s in r:
            try:
                data = s.recv(65536)
            except (BlockingIOError, InterruptedError):
                continue
            if not data:
                eof.add(s)
                pending.discard(s)
                continue
            out[s] += data
            got_any = True
    return out, eof


def send_all(sock, data, mbps, name):
    if mbps <= 0:
        sock.sendall(data)
        return
    step = max(1, int(mbps * 1024 * 1024 * COLLECT_S))
    off = 0
    while off < len(data):
        sock.sendall(data[off : off + step])
        off += step
        time.sleep(COLLECT_S)


class Direction:
    def __init__(self, name, mbps):
        self.name = name
        self.mbps = mbps
        self.total = 0
        self.delivered = 0
        self.dropped = 0.0
        self.iter = 0


def handle(conn, addr):
    """One thread per connection; owns both sockets for its lifetime."""
    try:
        backend = socket.create_connection((target_host, target_port), timeout=10)
    except Exception as e:
        log(f"[{addr}] backend connect failed: {e}")
        conn.close()
        return
    try:
        tls = ctx.wrap_socket(conn, server_side=True)
    except Exception as e:
        log(f"[{addr}] TLS handshake failed: {e}")
        conn.close()
        backend.close()
        return
    backend.settimeout(None)

    up = Direction("up", up_mbps)  # tls -> backend (raw)
    down = Direction("down", down_mbps)  # backend -> tls (WS-frame aware)
    unframed = b""  # downlink bytes not yet resolved to whole frames
    ws_started = False
    last_log = time.monotonic()
    start = time.monotonic()
    log(f"[{addr}] connected (tls ok), proxying {target_host}:{target_port}")

    def down_send(data):
        # Frame-aware loss model on the downlink.
        nonlocal unframed, ws_started
        unframed += data
        out = b""
        if not ws_started:
            idx = unframed.find(b"\r\n\r\n")
            if idx < 0:
                return
            out = unframed[: idx + 4]
            unframed = unframed[idx + 4:]
            ws_started = True
        while True:
            end = ws_frame_end(unframed)
            if end is None:
                break
            frame = unframed[:end]
            unframed = unframed[end:]
            if DROP_BYTES > 0 and down.delivered >= DROP_AFTER:
                if down.dropped < DROP_BYTES:
                    down.dropped += end
                    log(f"[down] dropped frame ({end}B), total {down.dropped:.0f}/{DROP_BYTES:.0f}")
                    continue
            out += frame
        if out:
            send_all(tls, out, down.mbps, "down")
            down.delivered += len(out)

    try:
        while True:
            got, eof = collect_burst([tls, backend])
            if eof:
                break
            up_data = got.get(tls, b"")
            down_data = got.get(backend, b"")
            if up_data:
                up.total += len(up_data)
                up.iter += 1
                time.sleep(rtt_ms / 1000.0)
                send_all(backend, up_data, up.mbps, "up")
            if down_data:
                down.total += len(down_data)
                down.iter += 1
                time.sleep(rtt_ms / 1000.0)
                down_send(down_data)
            now = time.monotonic()
            if now - last_log >= 5.0:
                log(
                    f"[{addr}] up: got={up.total} iters={up.iter} | "
                    f"down: got={down.total} delivered={down.delivered} "
                    f"unframed={len(unframed)} iters={down.iter}"
                )
                last_log = now
    except (ConnectionResetError, BrokenPipeError, ssl.SSLError, OSError) as e:
        log(f"[{addr}] connection ended: {type(e).__name__}: {e}")
    finally:
        try:
            tls.close()
        except Exception:
            pass
        try:
            backend.close()
        except Exception:
            pass
        log(f"[{addr}] done after {time.monotonic() - start:.1f}s")


def main():
    global ctx
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", listen_port))
    listener.listen(8)
    log(
        f"tls-wan proxy :{listen_port} (TLS end) -> {target_host}:{target_port} "
        f"rtt={rtt_ms:.0f}ms down={down_mbps}MB/s up={up_mbps}MB/s "
        f"drop_after={DROP_AFTER:.0f} drop_bytes={DROP_BYTES:.0f}"
    )
    while True:
        conn, addr = listener.accept()
        t = threading.Thread(target=handle, args=(conn, addr), daemon=True)
        t.start()


if __name__ == "__main__":
    main()
