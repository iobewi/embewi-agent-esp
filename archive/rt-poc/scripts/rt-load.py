#!/usr/bin/env python3
"""Charge de lecture sur l'API admin du device (POC 2d, expérience « plan de
contrôle réel »): des GET authentifiés, chacun sur une NOUVELLE connexion
(donc un handshake TLS complet par requête quand le device sert en HTTPS),
pour occuper le CPU/le radio du device pendant que le workload tourne à 10 kHz.

Uniquement des lectures: /v1alpha1/info et /v1alpha1/config. Volontairement
PAS /v1alpha1/health: son `self_check` fait un set/get/delete NVS, donc des
écritures flash -- c'est l'expérience suivante (gate POC 3), pas celle-ci.

Usage:
  scripts/rt-load.py <host> <token> [--https] [--port N] [--seconds 300]
                     [--rate R]   # requêtes/s max au total (0 = en continu)
                     [--workers 1]
"""
import argparse, socket, ssl, sys, threading, time

PATHS = ["/v1alpha1/info", "/v1alpha1/config"]


def one(host, port, tls, token, path, timeout):
    t0 = time.monotonic()
    s = socket.create_connection((host, port), timeout=timeout)
    try:
        if tls:
            s = ssl._create_unverified_context().wrap_socket(s, server_hostname=host)
        req = (f"GET {path} HTTP/1.1\r\nHost: {host}\r\n"
               f"Authorization: Bearer {token}\r\nConnection: close\r\n\r\n")
        s.sendall(req.encode())
        data = b""
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        status = int(data.split(b" ", 2)[1]) if data.startswith(b"HTTP/") else 0
        return status, time.monotonic() - t0, len(data)
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("token")
    ap.add_argument("--https", action="store_true")
    ap.add_argument("--port", type=int)
    ap.add_argument("--seconds", type=float, default=300)
    ap.add_argument("--rate", type=float, default=0)
    ap.add_argument("--workers", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=10)
    a = ap.parse_args()
    port = a.port or (443 if a.https else 80)
    deadline = time.monotonic() + a.seconds
    lock = threading.Lock()
    lat, codes, errors = [], {}, {}
    per_worker_gap = (a.workers / a.rate) if a.rate > 0 else 0

    def worker(idx):
        n = 0
        while time.monotonic() < deadline:
            t_start = time.monotonic()
            try:
                st, dt, _ = one(a.host, port, a.https, a.token, PATHS[(n + idx) % len(PATHS)], a.timeout)
                with lock:
                    lat.append(dt)
                    codes[st] = codes.get(st, 0) + 1
            except Exception as e:  # noqa: BLE001
                with lock:
                    errors[type(e).__name__] = errors.get(type(e).__name__, 0) + 1
                time.sleep(0.5)
            n += 1
            rest = per_worker_gap - (time.monotonic() - t_start)
            if rest > 0:
                time.sleep(rest)

    def pct(v, p):
        return v[min(len(v) - 1, int(len(v) * p))] if v else float("nan")

    threads = [threading.Thread(target=worker, args=(i,), daemon=True) for i in range(a.workers)]
    t_begin = time.monotonic()
    for t in threads:
        t.start()
    last = t_begin
    try:
        while any(t.is_alive() for t in threads):
            time.sleep(10)
            now = time.monotonic()
            with lock:
                v = sorted(lat)
                print(f"[{now - t_begin:6.0f}s] req={len(v)} ({len(v) / (now - t_begin):.1f}/s) codes={codes} "
                      f"errors={errors} lat p50={pct(v, .5) * 1000:.0f}ms p95={pct(v, .95) * 1000:.0f}ms "
                      f"max={(v[-1] * 1000 if v else 0):.0f}ms", flush=True)
            last = now
    except KeyboardInterrupt:
        pass
    for t in threads:
        t.join(timeout=a.timeout + 1)
    v = sorted(lat)
    print(f"FIN: {len(v)} requêtes en {time.monotonic() - t_begin:.0f}s, codes={codes}, erreurs={errors}, "
          f"p50={pct(v, .5) * 1000:.0f}ms p95={pct(v, .95) * 1000:.0f}ms max={(v[-1] * 1000 if v else 0):.0f}ms")
    sys.exit(0 if not errors and set(codes) <= {200} else 1)


if __name__ == "__main__":
    main()
