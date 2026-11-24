"""Load test of a built module on a local SpacetimeDB, with a fake slow store. Nothing leaves the machine.

The module's outbound calls go through `proxy`: every request waits `--delay` ms (changeable while it
runs, see `delay`), then a CONNECT (https) gets 502, which the module sees as a transport error, and a
plain request 200 {}. SpacetimeDB refuses loopback addresses as call targets, but takes the proxy from
the environment, so a store or game behind the proxy can be as slow as a test needs.

    python tools/loadtest.py proxy --delay 300                       # terminal 1
    HTTPS_PROXY=http://127.0.0.1:8899 HTTP_PROXY=http://127.0.0.1:8899 \\
        spacetime start --in-memory -l 127.0.0.1:3013                # terminal 2
    python tools/loadtest.py setup --wasm server/target/wasm32-unknown-unknown/release/purser.wasm
    python tools/loadtest.py run --route grant --conc 16 --secs 10    # writes, no outbound call
    python tools/loadtest.py run --route entitlements --conc 16       # reads
    python tools/loadtest.py delivery webhook                         # every grant now calls the slow "game"
    python tools/loadtest.py run --route grant --conc 64
    python tools/loadtest.py delay 15000                              # a store that hangs
    python tools/loadtest.py delivery none                            # the queue drains
    python tools/loadtest.py health

`setup` publishes a throwaway database with one build (`com.example.load`) and 40 server keys (the
`/server/*` routes stand in for a client purchase: a grant runs the same ledger steps, and a webhook
delivery is the outbound call). State is kept in `loadtest-state.json` next to the working directory.
Results print as one JSON line per run: answers per second, latency percentiles, status codes.
"""
import argparse
import asyncio
import hashlib
import http.client
import json
import multiprocessing as mp
import os
import random
import secrets
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

STATE = "loadtest-state.json"
DELAY_FILE = "loadtest-delay.txt"
APP = "com.example.load"
PLAYERS = 3000
KEYS = 40


def req(method, url, data=None, ctype="application/json", tok=None):
    h = {"Content-Type": ctype}
    if tok:
        h["Authorization"] = f"Bearer {tok}"
    r = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(r, timeout=300) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def load_state():
    return json.load(open(STATE))


def call(s, name, *args):
    code, body = req("POST", f"{s['base']}/v1/database/{s['db']}/call/{name}", json.dumps(list(args)).encode(), tok=s["tok"])
    if code != 200:
        sys.exit(f"{name}: {code} {body[:300]!r}")
    return body


def app_input(delivery):
    i = {"game_key": "load", "app_id": APP, "platform": "android", "delivery": delivery}
    if delivery == "webhook":
        i["webhook_url"] = "https://slow-game.example.com/hook"
    return json.dumps(i)


# --- proxy -------------------------------------------------------------------------------------

def proxy(a):
    stats = {"requests": 0, "in_flight": 0, "max_in_flight": 0}
    with open(DELAY_FILE, "w") as f:
        f.write(str(a.delay))

    def delay_s():
        try:
            return int(open(DELAY_FILE).read().strip()) / 1000
        except (OSError, ValueError):
            return a.delay / 1000

    async def handle(reader, writer):
        try:
            head = await reader.readuntil(b"\r\n\r\n")
        except (asyncio.IncompleteReadError, asyncio.LimitOverrunError, ConnectionError):
            writer.close()
            return
        stats["requests"] += 1
        stats["in_flight"] += 1
        stats["max_in_flight"] = max(stats["max_in_flight"], stats["in_flight"])
        try:
            await asyncio.sleep(delay_s())
            if head.startswith(b"CONNECT"):
                writer.write(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            else:
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            await writer.drain()
        except ConnectionError:
            pass
        finally:
            stats["in_flight"] -= 1
            writer.close()

    async def report():
        while True:
            await asyncio.sleep(5)
            print(f"{time.strftime('%H:%M:%S')} requests={stats['requests']} in_flight={stats['in_flight']} "
                  f"max_in_flight={stats['max_in_flight']}", flush=True)

    async def main():
        server = await asyncio.start_server(handle, "127.0.0.1", a.port, backlog=1024)
        print(f"slow proxy on 127.0.0.1:{a.port}, {a.delay} ms per call", flush=True)
        asyncio.create_task(report())
        async with server:
            await server.serve_forever()

    asyncio.run(main())


def delay(a):
    with open(DELAY_FILE, "w") as f:
        f.write(str(a.ms))
    print(f"proxy delay {a.ms} ms")


# --- setup -------------------------------------------------------------------------------------

def setup(a):
    base = a.server.rstrip("/")
    wasm = open(a.wasm, "rb").read()
    db = f"purser-load-{int(time.time())}"
    tok = json.loads(req("POST", base + "/v1/identity")[1])["token"]
    code, body = req("PUT", f"{base}/v1/database/{db}", wasm, "application/wasm", tok)
    if code != 200:
        sys.exit(f"publish {code} {body[:300]!r}")
    s = {"base": base, "db": db, "tok": tok, "keys": []}
    call(s, "admin_upsert_game_app", app_input("none"))
    call(s, "admin_upsert_products", json.dumps([
        {"app_id": APP, "product_id": "no_ads", "kind": "non_consumable"},
        {"app_id": APP, "product_id": "coins", "kind": "consumable"},
    ]))
    call(s, "admin_set_credential", APP, "webhook_secret", secrets.token_hex(16))
    for _ in range(KEYS):
        key_id = secrets.token_hex(6)
        key = f"psk_{key_id}_{secrets.token_urlsafe(32)}"
        call(s, "admin_add_server_key", json.dumps({"game_key": "load", "name": f"load-{key_id}", "scopes": "read,grant",
                                                    "key_id": key_id, "key_hash": hashlib.sha256(key.encode()).hexdigest()}))
        s["keys"].append(key)
    with open(STATE, "w") as f:
        json.dump(s, f)
    print(f"database {db}: build {APP}, {KEYS} server keys")


# --- run ---------------------------------------------------------------------------------------

def request_for(route, rnd, seq):
    user = f"player-{rnd.randrange(PLAYERS)}"
    if route == "entitlements":
        return "/entitlements", {"app_id": APP, "user_id": user}, False
    if route == "server_entitlements":
        return "/server/entitlements", {"app_id": APP, "user_id": user}, True
    if route == "grant":
        product = "no_ads" if seq % 3 == 0 else "coins"
        tx = f"t-{os.getpid()}-{threading.get_ident()}-{seq}-{rnd.getrandbits(32)}"
        return "/server/grant", {"app_id": APP, "user_id": user, "product_id": product, "transaction_id": tx}, True
    raise SystemExit(f"unknown route {route}")


def worker(args):
    s, route, threads, secs, seed = args
    u = urllib.parse.urlparse(s["base"])
    prefix = f"/v1/database/{s['db']}/route"
    out = []
    lock = threading.Lock()
    deadline = time.time() + secs

    def loop(tid):
        rnd = random.Random(seed * 1000 + tid)
        conn = http.client.HTTPConnection(u.hostname, u.port, timeout=120)
        seq, mine = 0, []
        while time.time() < deadline:
            path, body, keyed = request_for(route, rnd, seq)
            seq += 1
            h = {"Content-Type": "application/json"}
            if keyed:
                h["Authorization"] = "Bearer " + rnd.choice(s["keys"])
            t0 = time.perf_counter()
            try:
                conn.request("POST", prefix + path, json.dumps(body), h)
                r = conn.getresponse()
                r.read()
                status = r.status
            except (OSError, http.client.HTTPException):
                status = -1
                conn.close()
                conn = http.client.HTTPConnection(u.hostname, u.port, timeout=120)
            mine.append((status, time.perf_counter() - t0))
        with lock:
            out.extend(mine)

    ts = [threading.Thread(target=loop, args=(i,)) for i in range(threads)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return out


def run(a):
    s = load_state()
    procs = max(1, min(a.conc, a.procs))
    per = [a.conc // procs + (1 if i < a.conc % procs else 0) for i in range(procs)]
    t0 = time.time()
    with mp.Pool(procs) as pool:
        parts = pool.map(worker, [(s, a.route, n, a.secs, i + int(t0)) for i, n in enumerate(per)])
    res = [x for p in parts for x in p]
    codes = {}
    for c, _ in res:
        codes[c] = codes.get(c, 0) + 1
    lat = sorted(d for c, d in res if c == 200)
    q = lambda p: round(lat[min(len(lat) - 1, int(p * len(lat)))] * 1000, 1) if lat else None
    print(json.dumps({"route": a.route, "conc": a.conc, "secs": a.secs, "ok_per_s": round(len(lat) / a.secs, 1),
                      "p50_ms": q(0.5), "p90_ms": q(0.9), "p99_ms": q(0.99), "codes": codes}), flush=True)


def delivery(a):
    call(load_state(), "admin_upsert_game_app", app_input(a.mode))
    print("delivery", a.mode)


def health(_a):
    s = load_state()
    _, body = req("GET", f"{s['base']}/v1/database/{s['db']}/route/healthz")
    print(time.strftime("%H:%M:%S"), body.decode())


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    x = sub.add_parser("proxy", help="the fake slow store / game")
    x.add_argument("--port", type=int, default=8899)
    x.add_argument("--delay", type=int, default=300, help="ms per call")
    x = sub.add_parser("delay", help="change the proxy's delay while it runs")
    x.add_argument("ms", type=int)
    x = sub.add_parser("setup", help="publish a throwaway database and configure it")
    x.add_argument("--server", default="http://127.0.0.1:3013")
    x.add_argument("--wasm", required=True)
    x = sub.add_parser("run", help="load one route")
    x.add_argument("--route", required=True, choices=["entitlements", "server_entitlements", "grant"])
    x.add_argument("--conc", type=int, default=8, help="requests at once")
    x.add_argument("--secs", type=float, default=10)
    x.add_argument("--procs", type=int, default=8, help="client processes")
    x = sub.add_parser("delivery", help="the build's delivery: none (no outbound call) or webhook (through the proxy)")
    x.add_argument("mode", choices=["none", "webhook"])
    sub.add_parser("health", help="print /healthz")
    a = p.parse_args()
    {"proxy": proxy, "delay": delay, "setup": setup, "run": run, "delivery": delivery, "health": health}[a.cmd](a)
