#!/usr/bin/env python3
"""Faux plan de contrôle Embewi pour l'expérience POC 2d « plan de contrôle
réel »: un serveur TLS qui répond à ce que le device sortant lui envoie.

  POST /v1alpha1/heartbeat   -> 200 {"ok":true}
  GET  /v1alpha1/logs (WS)   -> 101 Switching Protocols, puis lit et compte
                                les trames de log jusqu'à la fermeture

À lancer sur la machine dont l'IP est le `ctrl_url` du device
(192.168.100.133 dans nos tests), avec un cert signé par le CA poussé sur le
device (POST /v1alpha1/tls/ca) et dont le SAN contient cette IP:

  python3 scripts/ctrl-mock.py --cert ctrl.pem --key ctrl.key [--port 8443]
"""
import argparse, base64, hashlib, socket, ssl, struct, threading, time

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
stats = {"heartbeats": 0, "ws_sessions": 0, "ws_frames": 0, "ws_bytes": 0, "errors": 0}
lock = threading.Lock()


def bump(k, n=1):
    with lock:
        stats[k] += n


def read_headers(conn):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = conn.recv(4096)
        if not chunk:
            return None, b""
        buf += chunk
        if len(buf) > 65536:
            return None, b""
    head, _, rest = buf.partition(b"\r\n\r\n")
    return head.decode("latin1"), rest


def read_exact(conn, n, prefix=b""):
    buf = prefix
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf[:n], buf[n:]


def handle(conn):
    try:
        head, rest = read_headers(conn)
        if head is None:
            return
        lines = head.split("\r\n")
        method, path = lines[0].split(" ")[:2]
        hdrs = {l.split(":", 1)[0].strip().lower(): l.split(":", 1)[1].strip() for l in lines[1:] if ":" in l}
        if method == "POST" and path.startswith("/v1alpha1/heartbeat"):
            n = int(hdrs.get("content-length", "0"))
            read_exact(conn, n, rest)
            body = b'{"ok":true}'
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                         b"Content-Length: %d\r\nConnection: close\r\n\r\n%s" % (len(body), body))
            bump("heartbeats")
        elif method == "GET" and path.startswith("/v1alpha1/logs") and "sec-websocket-key" in hdrs:
            accept = base64.b64encode(hashlib.sha1(hdrs["sec-websocket-key"].encode() + GUID).digest())
            conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                         b"Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
            bump("ws_sessions")
            buf = rest
            while True:
                hdr, buf = read_exact(conn, 2, buf)
                opcode, ln = hdr[0] & 0x0F, hdr[1] & 0x7F
                if ln == 126:
                    ext, buf = read_exact(conn, 2, buf)
                    ln = struct.unpack(">H", ext)[0]
                elif ln == 127:
                    ext, buf = read_exact(conn, 8, buf)
                    ln = struct.unpack(">Q", ext)[0]
                if hdr[1] & 0x80:
                    _, buf = read_exact(conn, 4, buf)
                payload, buf = read_exact(conn, ln, buf)
                if opcode == 8:
                    break
                bump("ws_frames")
                bump("ws_bytes", len(payload))
        else:
            conn.sendall(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    except (EOFError, ConnectionError, ssl.SSLError, OSError):
        bump("errors")
    finally:
        try:
            conn.close()
        except OSError:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cert", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--port", type=int, default=8443)
    a = ap.parse_args()
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(a.cert, a.key)
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", a.port))
    srv.listen(8)
    print(f"ctrl-mock: TLS sur 0.0.0.0:{a.port}", flush=True)

    def reporter():
        while True:
            time.sleep(10)
            with lock:
                print(time.strftime("%H:%M:%S"), dict(stats), flush=True)

    threading.Thread(target=reporter, daemon=True).start()
    while True:
        raw, _ = srv.accept()
        try:
            conn = ctx.wrap_socket(raw, server_side=True)
        except (ssl.SSLError, OSError):
            bump("errors")
            raw.close()
            continue
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
