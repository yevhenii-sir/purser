"""End-to-end check of a built module on a real SpacetimeDB server (local or the CI's container).

    spacetime start --in-memory -l 127.0.0.1:3013          # or: docker run ... clockworklabs/spacetime start
    python tools/e2e.py --server http://127.0.0.1:3013 --wasm server/target/wasm32-unknown-unknown/release/purser.wasm

It creates a throwaway identity, publishes the module as it, configures an app with SQL-free admin
calls, grants purchases, then checks what unit tests cannot: the routes over HTTP (`/healthz`, the
gzipped admin page and its ETag, `/entitlements`, `/server/entitlements` with a server key), the
owner's `/sql` with `OR`, and a backup → `--delete-data` publish → restore round trip with
`server/backup.py` (paged reads, the ledger reindex). With `--game-wasm` (the reference receiver,
`server/game-receiver`) it also publishes a game database and delivers a grant to it through an
inter-database call (`delivery = spacetimedb_idc`). Exit code 1 on the first failure.
"""
import argparse
import gzip
import hashlib
import json
import os
import secrets
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--server", default="http://127.0.0.1:3000")
    p.add_argument("--wasm", required=True)
    p.add_argument("--game-wasm", help="server/game-receiver's wasm: check delivery through inter-database calls")
    a = p.parse_args()
    base = a.server.rstrip("/")
    wasm = open(a.wasm, "rb").read()
    db = f"purser-e2e-{int(time.time())}"

    def req(method, url, data=None, ctype="application/json", tok=None, headers=None):
        h = {"Content-Type": ctype, **(headers or {})}
        if tok:
            h["Authorization"] = f"Bearer {tok}"
        r = urllib.request.Request(url, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(r, timeout=120) as resp:
                return resp.status, {k.lower(): v for k, v in resp.headers.items()}, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()

    def check(cond, what):
        print(("ok    " if cond else "FAIL  ") + what)
        if not cond:
            sys.exit(1)

    for _ in range(90):
        try:
            urllib.request.urlopen(base + "/v1/ping", timeout=5)
            break
        except Exception:
            time.sleep(1)
    else:
        sys.exit(f"no SpacetimeDB at {base}")

    tok = json.loads(req("POST", base + "/v1/identity")[2])["token"]
    code, _, body = req("PUT", f"{base}/v1/database/{db}", wasm, "application/wasm", tok)
    check(code == 200, f"publish ({code} {body[:200]!r})")
    route = f"{base}/v1/database/{db}/route"

    def call(name, *args):
        code, _, body = req("POST", f"{base}/v1/database/{db}/call/{name}", json.dumps(list(args)).encode(), tok=tok)
        if code != 200:
            check(False, f"{name} ({code} {body[:200]!r})")
        return body

    def sql(q):
        code, _, body = req("POST", f"{base}/v1/database/{db}/sql", q.encode(), "text/plain", tok)
        if code != 200:
            check(False, f"sql {q[:60]} ({code} {body[:200]!r})")
        return json.loads(body)[0]

    app = "com.example.e2e"
    call("admin_upsert_game_app", json.dumps({"game_key": "e2e", "app_id": app, "platform": "android", "delivery": "none"}))
    call("admin_upsert_products", json.dumps([
        {"app_id": app, "product_id": "vip", "kind": "subscription", "access": "premium"},
        {"app_id": app, "product_id": "no_ads", "kind": "non_consumable"},
    ]))
    call("admin_upsert_alert_channel", json.dumps({"name": "ops", "kind": "webhook", "enabled": False, "settings": {"url": "https://hooks.example.com/e2e"}}))
    call("admin_set_setting", "retention_days", "365")
    for i in range(30):
        call("admin_grant_manual", json.dumps({"app_id": app, "user_id": f"player-{i}", "product_id": "no_ads", "transaction_id": f"g-{i}"}))

    # Routes over HTTP.
    code, _, body = req("GET", f"{route}/healthz")
    health = json.loads(body)
    check(code == 200 and "pending_purchases" in health, f"/healthz {health}")
    code, h, body = req("GET", f"{route}/admin", headers={"Accept-Encoding": "gzip"})
    check(code == 200 and h.get("content-encoding") == "gzip" and gzip.decompress(body).startswith(b"<!doctype html>"), "admin page gzipped")
    code, _, _ = req("GET", f"{route}/admin", headers={"If-None-Match": h.get("etag", "")})
    check(code == 304, "admin page 304 by ETag")
    code, _, body = req("POST", f"{route}/entitlements", json.dumps({"app_id": app, "user_id": "player-3"}).encode())
    check(code == 200 and json.loads(body)["non_consumables"] == ["no_ads"], f"/entitlements {body[:200]!r}")
    rows = sql(f"SELECT app_id FROM game_app WHERE app_id = '{app}' OR app_id = 'none'")["rows"]
    check(len(rows) == 1, "/sql with OR")

    # A server key, made here as the admin page does.
    key_id = secrets.token_hex(6)
    key = f"psk_{key_id}_{secrets.token_urlsafe(32)}"
    call("admin_add_server_key", json.dumps({"game_key": "e2e", "name": "ci", "scopes": "read", "key_id": key_id, "key_hash": hashlib.sha256(key.encode()).hexdigest()}))
    code, _, body = req("POST", f"{route}/server/entitlements", json.dumps({"app_id": app, "user_id": "player-4"}).encode(), headers={"Authorization": f"Bearer {key}"})
    check(code == 200 and json.loads(body)["non_consumables"] == ["no_ads"], f"/server/entitlements {body[:200]!r}")
    code, _, _ = req("POST", f"{route}/server/entitlements", json.dumps({"app_id": app, "user_id": "player-4"}).encode())
    check(code == 401, "/server/entitlements without a key is refused")

    if a.game_wasm:
        idc(a, base, tok, db, req, check, call, sql)

    # Backup, a --delete-data publish of the same name, restore.
    tables = ["game_app", "product", "purchase", "purchase_log", "admin", "credential", "alert_channel", "purchase_count", "server_key"]
    before = {t: len(sql(f"SELECT * FROM {t}")["rows"]) for t in tables}
    # Contents, not only counts: rows restored as they were (the log without its renumbered ids).
    same = ["game_app", "product", "purchase", "server_key"]
    content = lambda: {t: sorted(json.dumps(r, sort_keys=True) for r in sql(f"SELECT * FROM {t}")["rows"]) for t in same}
    log_rows = lambda: sorted(json.dumps(r[1:]) for r in sql("SELECT * FROM purchase_log")["rows"])
    content_before, log_before = content(), log_rows()
    out = tempfile.mkdtemp(prefix="purser-e2e-")
    env = dict(os.environ, PURSER_TOKEN=tok)
    run = lambda *args: subprocess.run([sys.executable, "backup.py", *args], cwd=os.path.join(ROOT, "server"), env=env, capture_output=True, text=True)
    b = run("backup", "--server", base, "--database", db, "--dir", out, "--force")
    check(b.returncode == 0, f"backup.py backup {b.stderr[-500:]}")
    code, _, body = req("PUT", f"{base}/v1/database/{db}?clear=true", wasm, "application/wasm", tok)
    check(code == 200, f"publish --delete-data ({code} {body[:200]!r})")
    check(len(sql("SELECT * FROM purchase")["rows"]) == 0, "the database is empty after --delete-data")
    r = run("restore", "--server", base, "--database", db, "--dir", out)
    check(r.returncode == 0, "backup.py restore" + ("" if r.returncode == 0 else f" {r.stdout[-300:]} {r.stderr[-500:]}"))
    after = {t: len(sql(f"SELECT * FROM {t}")["rows"]) for t in tables}
    check(after == before, f"rows after the restore {after} == {before}")
    changed = [t for t, rows in content().items() if rows != content_before[t]]
    if log_rows() != log_before:
        changed.append("purchase_log")
    check(not changed, "restored rows are the same as backed up" + (f" (differ: {', '.join(changed)})" if changed else ""))
    # New rows of auto-increment tables (the log, the audit) get ids that do not collide with restored ones.
    call("admin_grant_manual", json.dumps({"app_id": app, "user_id": "player-9", "product_id": "no_ads", "transaction_id": "after-restore"}))
    check(len(sql("SELECT * FROM purchase_log")["rows"]) > before["purchase_log"], "a grant after the restore writes the log (no id collision)")
    code, _, body = req("POST", f"{route}/entitlements", json.dumps({"app_id": app, "user_id": "player-3"}).encode())
    check(code == 200 and json.loads(body)["non_consumables"] == ["no_ads"], "/entitlements after the restore (owned purchases reindexed)")
    print("done")


def idc(a, base, tok, db, req, check, call, sql):
    """A game database on the same server gets a grant through an inter-database call, once."""
    game_db = f"{db}-game"
    code, _, body = req("PUT", f"{base}/v1/database/{game_db}", open(a.game_wasm, "rb").read(), "application/wasm", tok)
    check(code == 200, f"publish the game receiver ({code} {body[:200]!r})")
    game = json.loads(body)["Success"]["database_identity"]
    billing = json.loads(req("GET", f"{base}/v1/database/{db}")[2])["database_identity"]
    if isinstance(billing, dict):  # SATS JSON: {"__identity__": "0x..."}
        billing = next(iter(billing.values()))
    billing = billing[2:] if billing.startswith("0x") else billing
    code, _, body = req("POST", f"{base}/v1/database/{game_db}/call/allow_purser", json.dumps([{"__identity__": "0x" + billing}]).encode(), tok=tok)
    check(code == 200, f"the game allows billing's identity ({code} {body[:200]!r})")
    app = "com.example.e2e.idc"
    call("admin_upsert_game_app", json.dumps({"game_key": "e2e", "app_id": app, "platform": "android", "delivery": "spacetimedb_idc", "spacetime_db": game}))
    call("admin_upsert_products", json.dumps([{"app_id": app, "product_id": "no_ads", "kind": "non_consumable"}]))
    call("admin_grant_manual", json.dumps({"app_id": app, "user_id": "player-idc", "product_id": "no_ads", "transaction_id": "idc-1"}))
    rows, status = [], ""
    for _ in range(60):
        code, _, body = req("POST", f"{base}/v1/database/{game_db}/sql", b"SELECT * FROM billing_event_log", "text/plain", tok)
        rows = json.loads(body)[0]["rows"] if code == 200 else []
        status = sql(f"SELECT status FROM purchase WHERE id = 'manual:e2e.idc-1'")["rows"]
        if rows and status and status[0][0] == "completed":
            break
        time.sleep(1)
    check(len(rows) == 1 and "grant" in rows[0] and "manual:e2e.idc-1#" in json.dumps(rows[0]), f"the game database got the grant once, by event_id: {rows}")
    check(status and status[0][0] == "completed", f"billing applied the game's answer: {status}")


if __name__ == "__main__":
    main()
