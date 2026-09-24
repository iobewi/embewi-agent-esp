#!/usr/bin/env python3
"""Mock "Core" for the 3C heartbeat keep-alive and 4A log_stream backoff
hardware gates -- and a live viewer for the device's ESP_LOG stream over
the network, no serial cable needed.

Listens HTTPS on 0.0.0.0:8443 (matching the device's ctrl_url):
  - POST /v1alpha1/heartbeat: small JSON ack, HTTP/1.1 keep-alive (3C).
  - GET  /v1alpha1/logs (WS upgrade): accepts the handshake, then either
    stays open and decodes/prints every log frame the device sends
    (default -- see src/log_stream.rs: one WS Text frame per line, masked
    per RFC 6455 since the device is the client here, JSON payload
    `{ts, node, workload, level, msg}`) or closes the TCP connection
    immediately after accepting (--close-after-ws-accept -- for 4A's
    "Core accepts then closes immediately" scenario).

Logs every request/connection with a per-connection id and a per-connection
heartbeat sequence number, so "N heartbeats, 1 connection" or "WS accepted,
then closed" is directly visible in the output.

Usage:
    python3 mock_core.py [--cert server.pem] [--key server.key] [--port 8443]
                          [--close-after-ws-accept] [--raw-frames]

Ctrl-C stops it; run it again to simulate "Core restarts" for the gate.
"""
import argparse
import base64
import hashlib
import http.server
import itertools
import json
import ssl
import struct
import sys
import time

conn_counter = itertools.count(1)
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# WS opcodes (RFC 6455 §5.2) this mock cares about.
OP_TEXT = 0x1
OP_CLOSE = 0x8
OP_PING = 0x9
OP_PONG = 0xA


def ts():
    return time.strftime("%H:%M:%S")


def read_ws_frame(rfile):
    """Reads one RFC 6455 frame from a client (always masked, since the
    device is the WS client here -- see log_stream.rs's `pump_session`,
    which masks every frame it sends). Returns (opcode, payload_bytes),
    or None on a clean EOF. No fragmentation support: log_stream.rs never
    sends `FrameType::Text(true-as-continuation)`, only whole single-frame
    lines, so continuation frames (opcode 0x0) are out of scope here.
    """
    head = rfile.read(2)
    if len(head) < 2:
        return None
    b1, b2 = head
    opcode = b1 & 0x0F
    masked = b2 & 0x80
    length = b2 & 0x7F
    if length == 126:
        length = struct.unpack(">H", rfile.read(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", rfile.read(8))[0]
    mask = rfile.read(4) if masked else None
    payload = rfile.read(length)
    if len(payload) < length:
        return None
    if mask:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, payload


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"  # required for http.server to offer keep-alive at all
    close_after_ws_accept = False  # set from argv in main()
    raw_frames = False  # set from argv in main()

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

        self.close_connection = False
        try:
            while True:
                frame = read_ws_frame(self.rfile)
                if frame is None:
                    break
                opcode, payload = frame
                if opcode == OP_CLOSE:
                    break
                if opcode != OP_TEXT:
                    continue  # Ping/Pong: log_stream.rs only ever answers
                    # pings the mock might send, never sends its own -- and
                    # this mock never sends any, so these don't occur in
                    # practice. Ignored rather than answered to keep this
                    # loop doing one thing.
                if Handler.raw_frames:
                    print(f"[{ts()}] conn #{self.conn_id} log: {payload!r}", flush=True)
                    continue
                try:
                    line = json.loads(payload)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    print(f"[{ts()}] conn #{self.conn_id} log: <undecodable> {payload!r}", flush=True)
                    continue
                print(
                    f"[{ts()}] conn #{self.conn_id} log: "
                    f"[{line.get('level', '?')}] {line.get('node', '?')}: {line.get('msg', '')}",
                    flush=True,
                )
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
    p.add_argument(
        "--raw-frames",
        action="store_true",
        help="Print each log WS frame's raw bytes instead of decoding it as {ts,node,workload,level,msg} JSON.",
    )
    args = p.parse_args()
    Handler.close_after_ws_accept = args.close_after_ws_accept
    Handler.raw_frames = args.raw_frames

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
