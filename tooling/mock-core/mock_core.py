#!/usr/bin/env python3
"""Mock "Core" for the 3C heartbeat keep-alive and 4A log_stream backoff
hardware gates.

Listens HTTPS on 0.0.0.0:8443 (matching the device's ctrl_url):
  - POST /v1alpha1/heartbeat: small JSON ack, HTTP/1.1 keep-alive (3C).
  - GET  /v1alpha1/logs (WS upgrade): accepts the handshake, then either
    stays open (default -- for 4A's "stable session" scenarios) or closes
    the TCP connection immediately after accepting (--close-after-ws-accept
    -- for 4A's "Core accepts then closes immediately" scenario).

Logs every request/connection with a per-connection id and a per-connection
heartbeat sequence number, so "N heartbeats, 1 connection" or "WS accepted,
then closed" is directly visible in the output.

Usage:
    python3 mock_core.py [--cert server.pem] [--key server.key] [--port 8443]
                          [--close-after-ws-accept]

Ctrl-C stops it; run it again to simulate "Core restarts" for the gate.
"""
import argparse
import base64
import hashlib
import http.server
import itertools
import json
import ssl
import sys
import time

conn_counter = itertools.count(1)
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def ts():
    return time.strftime("%H:%M:%S")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"  # required for http.server to offer keep-alive at all
    close_after_ws_accept = False  # set from argv in main()

    def setup(self):
        super().setup()
        self.conn_id = next(conn_counter)
        self.seq = 0
        print(f"[{ts()}] conn #{self.conn_id}: opened from {self.client_address}", flush=True)

    def finish(self):
        print(f"[{ts()}] conn #{self.conn_id}: closed after {self.seq} heartbeat(s)", flush=True)
        super().finish()

    def log_message(self, fmt, *args):
        pass  # replaced by our own logging in do_GET/do_POST/setup/finish

    def do_GET(self):
        if self.path != "/v1alpha1/logs" or self.headers.get("Upgrade", "").lower() != "websocket":
            self.send_response(404)
            self.end_headers()
            return

        key = self.headers.get("Sec-WebSocket-Key", "")
        accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
        self.send_response(101, "Switching Protocols")
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        print(f"[{ts()}] conn #{self.conn_id}: WS upgrade accepted", flush=True)

        if Handler.close_after_ws_accept:
            print(f"[{ts()}] conn #{self.conn_id}: closing immediately (--close-after-ws-accept)", flush=True)
            self.close_connection = True
            return

        # Not a real WS frame parser -- just drains whatever the client
        # sends (log frames, the occasional Ping) without decoding it, so
        # the connection stays open and doesn't back up. Good enough here:
        # this gate only cares whether the TCP/TLS connection stays up,
        # not what's said over it.
        self.close_connection = False
        try:
            while True:
                data = self.rfile.read(4096)
                if not data:
                    break
        except (ConnectionError, OSError):
            pass
        print(f"[{ts()}] conn #{self.conn_id}: WS connection ended", flush=True)

    def do_POST(self):
        self.seq += 1
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        try:
            parsed = json.loads(body) if body else {}
        except json.JSONDecodeError:
            parsed = {"_raw": body[:80]}
        print(
            f"[{ts()}] conn #{self.conn_id} heartbeat #{self.seq}: "
            f"{self.path} node={parsed.get('node_id')} state={parsed.get('state')} "
            f"heap_free={parsed.get('heap_free')}",
            flush=True,
        )
        resp = json.dumps({"status": "ok"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        # No explicit Connection header: HTTP/1.1 defaults to keep-alive,
        # which is exactly the case this gate needs exercised.
        self.end_headers()
        self.wfile.write(resp)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--cert", default="server.pem")
    p.add_argument("--key", default="server.key")
    p.add_argument("--port", type=int, default=8443)
    p.add_argument(
        "--close-after-ws-accept",
        action="store_true",
        help="Accept the WS upgrade, then close immediately (4A scenario 4).",
    )
    args = p.parse_args()
    Handler.close_after_ws_accept = args.close_after_ws_accept

    server = http.server.ThreadingHTTPServer(("0.0.0.0", args.port), Handler)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(certfile=args.cert, keyfile=args.key)
    server.socket = ctx.wrap_socket(server.socket, server_side=True)
    mode = "close-after-ws-accept" if args.close_after_ws_accept else "normal"
    print(f"[{ts()}] mock Core listening on :{args.port} (cert={args.cert}, mode={mode})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print(f"[{ts()}] stopped", flush=True)
        sys.exit(0)


if __name__ == "__main__":
    main()
