"""Builds the data of the admin page demo (`server/admin-ui/demo/snapshot.json`).

Publishes the module to a local SpacetimeDB, fills it with made-up apps, products, a year of
purchases, integrations and a paywall A/B test, then saves what the admin page reads: the tables
(secret values are never read, only their names) and the answers of the admin procedures. The
demo build of the page (`npm run build:demo`) answers from this file instead of a server.

    spacetime start --in-memory -l 127.0.0.1:3013
    cd server && cargo build -p purser --target wasm32-unknown-unknown --release --locked
    python tools/demo_snapshot.py server/target/wasm32-unknown-unknown/release/purser.wasm

Everything here is fictional (example.com ids, random players and orders).
"""
import argparse
import datetime as dt
import json
import os
import random
import time
import urllib.error
import urllib.request

DAY_MS = 86_400_000
DAY_US = DAY_MS * 1000
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "server", "admin-ui", "demo", "snapshot.json")


class Server:
    def __init__(self, base, db):
        self.base, self.db, self.token = base.rstrip("/"), db, ""

    def req(self, method, path, body=None, ctype="application/json", auth=True):
        data = body if isinstance(body, (bytes, type(None))) else body.encode()
        r = urllib.request.Request(self.base + path, data=data, method=method)
        if auth and self.token:
            r.add_header("Authorization", "Bearer " + self.token)
        if data is not None:
            r.add_header("Content-Type", ctype)
        try:
            with urllib.request.urlopen(r, timeout=120) as resp:
                return resp.status, resp.read().decode()
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode()

    def call(self, name, *args):
        s, out = self.req("POST", f"/v1/database/{self.db}/call/{name}", json.dumps(list(args)))
        if s >= 300:
            raise SystemExit(f"{name}: {s} {out[:300]}")
        return out

    def proc(self, name, *args):
        v = json.loads(self.call(name, *args))
        return json.loads(v) if isinstance(v, str) else v

    def sql(self, q):
        s, out = self.req("POST", f"/v1/database/{self.db}/sql", q, "text/plain")
        if s >= 300:
            raise SystemExit(f"sql {q}: {s} {out[:300]}")
        res = json.loads(out)[0]
        cols = [str(e["name"].get("some", e["name"]) if isinstance(e["name"], dict) else e["name"]) for e in res["schema"]["elements"]]
        cols = [__import__("re").sub(r"([a-z])_(\d)", r"\1\2", c) for c in cols]

        def unwrap(v):
            return v[0] if isinstance(v, list) and len(v) == 1 and not isinstance(v[0], (dict, list)) else v

        return [{c: unwrap(v) for c, v in zip(cols, row)} for row in res["rows"]]

    def route(self, path, body):
        return self.req("POST", f"/v1/database/{self.db}/route{path}", json.dumps(body), auth=False)


# --- The made-up catalog ---------------------------------------------------------------------

GAMES = {
    "galaxy-miner": {
        "builds": [
            ("com.example.galaxyminer", "android", "", "google"),
            ("com.example.galaxyminer.ios", "ios", "", "apple"),
            ("com.example.galaxyminer.amazon", "amazon", "", "amazon"),
        ],
        "products": [
            ("gems_small", "consumable", 0.99, 30),
            ("gems_medium", "consumable", 4.99, 18),
            ("gems_large", "consumable", 19.99, 5),
            ("no_ads", "non_consumable", 2.99, 9),
            ("vip_monthly", "subscription", 7.99, 12),
            ("vip_yearly", "subscription", 49.99, 2),
        ],
    },
    "word-garden": {
        "builds": [
            ("com.example.wordgarden", "android", "", "google"),
            ("com.example.wordgarden.ios", "ios", "", "apple"),
            ("com.example.wordgarden.web", "web", "stripe", "stripe"),
        ],
        "products": [
            ("hints_10", "consumable", 1.99, 25),
            ("hints_50", "consumable", 6.99, 8),
            ("premium", "non_consumable", 9.99, 6),
            ("plus_monthly", "subscription", 4.99, 10),
        ],
    },
}
COUNTRIES = [("US", "USD", 1.0, 34), ("GB", "GBP", 0.79, 8), ("DE", "EUR", 0.92, 9), ("FR", "EUR", 0.92, 6),
             ("BR", "BRL", 5.0, 9), ("JP", "JPY", 150.0, 7), ("CA", "CAD", 1.36, 6), ("AU", "AUD", 1.52, 4),
             ("IN", "INR", 83.0, 7), ("PL", "PLN", 3.95, 4), ("ES", "EUR", 0.92, 4), ("MX", "MXN", 17.0, 2)]


def tx_id(store, rnd):
    if store == "google":
        return "GPA.33%02d-%04d-%04d-%05d" % (rnd.randrange(100), rnd.randrange(10**4), rnd.randrange(10**4), rnd.randrange(10**5))
    if store == "apple":
        return "2000000%09d" % rnd.randrange(10**9)
    if store == "amazon":
        return "amzn1.receipt.%016x" % rnd.getrandbits(64)
    return "cs_live_%024x" % rnd.getrandbits(96)


def local_price(usd, rate, ccy):
    v = usd * rate
    v = round(v) - 0.01 if ccy not in ("JPY",) and v > 2 else round(v, 2)
    if ccy == "JPY":
        v = round(v / 10) * 10
    return int(round(v * 1e6))


def make_purchases(now_ms, rnd):
    """A year of purchases that grows over time; subscriptions renew monthly until they churn."""
    rows, seq = [], 0
    builds = [(g, b) for g, d in GAMES.items() for b in d["builds"]]
    weights = [6, 4, 1, 5, 3, 2]
    users, home = {}, {}

    def user(game):
        """A returning or a new player; a player keeps one country (and currency)."""
        pool = users.setdefault(game, [])
        if pool and rnd.random() < 0.8:
            return rnd.choice(pool)
        u = "player-%06x" % rnd.getrandbits(24)
        pool.append(u)
        home[u] = rnd.choices(COUNTRIES, [c[3] for c in COUNTRIES])[0]
        if rnd.random() < 0.03:
            testers.add(u)
        return u

    testers, owned, subscribed = set(), set(), {}

    def row(app, store, product, kind, uid, created_ms, price, ccy, country, reason="purchase", orig=None, expires=0, trial=False, status="completed"):
        nonlocal seq
        seq += 1
        tx = tx_id(store, rnd)
        r = {
            "id": f"{store}:{tx}", "store": store, "app_id": app, "product_id": product, "kind": kind,
            "transaction_id": tx, "original_transaction_id": orig or tx, "purchase_token": tx if store != "google" else "tok-%x" % rnd.getrandbits(48),
            "user_id": uid, "status": status, "reason": reason, "expires_at_ms": expires,
            "environment": "sandbox" if uid in testers else "production", "price_micros": 0 if trial else price,
            "currency": ccy, "created_at_micros": created_ms * 1000, "updated_at_micros": created_ms * 1000,
            "country": country, "was_granted": status != "rejected", "trial": trial,
        }
        rows.append(r)
        return r

    for day in range(365, -1, -1):
        growth = 0.35 + 0.65 * (365 - day) / 365
        weekend = 1.25 if dt.datetime.fromtimestamp((now_ms - day * DAY_MS) / 1000, dt.timezone.utc).weekday() >= 5 else 1.0
        n = max(0, int(rnd.gauss(6.5 * growth * weekend, 1.8)))
        for _ in range(n):
            (game, (app, platform, _, store)) = rnd.choices(builds, weights)[0]
            prods = GAMES[game]["products"]
            pid, kind, usd, _w = rnd.choices(prods, [p[3] for p in prods])[0]
            uid = user(game)
            t = now_ms - day * DAY_MS - rnd.randrange(DAY_MS)
            # A non-consumable is bought once, a subscription is not bought again while it runs.
            if (kind == "non_consumable" and (uid, game, pid) in owned) or (kind == "subscription" and subscribed.get((uid, game), 0) > t):
                pid, kind, usd, _w = prods[0]
            if kind == "non_consumable":
                owned.add((uid, game, pid))
            country, ccy, rate, _cw = home[uid]
            if store == "stripe":
                ccy, rate = ("USD", 1.0) if ccy not in ("EUR", "GBP") else (ccy, rate)
            price = local_price(usd, rate, ccy)
            if t > now_ms - 60_000:
                continue
            if kind != "subscription":
                st = rnd.choices(["completed", "revoked"], [97, 3])[0]
                row(app, store, pid, kind, uid, t, price, ccy, country, status=st)
                continue
            period = 365 if "yearly" in pid else 30
            trial = period == 30 and rnd.random() < 0.45
            first = row(app, store, pid, kind, uid, t, price, ccy, country, trial=trial, expires=t + (3 if trial else period) * DAY_MS)
            chain = [first]
            t2 = first["expires_at_ms"]
            keep = 0.62 if trial else 0.82
            if trial and rnd.random() > keep:
                pass
            else:
                while t2 < now_ms:
                    r = row(app, store, pid, kind, uid, t2, price, ccy, country, reason="renewal", orig=first["transaction_id"], expires=t2 + period * DAY_MS)
                    chain.append(r)
                    t2 = r["expires_at_ms"]
                    if rnd.random() > 0.86:
                        break
            subscribed[(uid, game)] = chain[-1]["expires_at_ms"]
            for c in chain:
                c["status"] = "completed" if c["expires_at_ms"] > now_ms else "expired"
            if rnd.random() < 0.02:
                chain[-1]["status"] = "revoked"
    # A few purchases still being delivered, for the Overview.
    for r in rows[-400:]:
        if r["status"] == "completed" and r["app_id"] == "com.example.wordgarden" and len([x for x in rows if x.get("attempts")]) < 3 and rnd.random() < 0.05:
            r.update(status="granted" if rnd.random() < 0.5 else "verified", attempts=3 + rnd.randrange(3),
                     next_attempt_at_micros=(now_ms + 4 * 60_000) * 1000,
                     last_error="delivery: webhook answered HTTP 503 (api.example.com)")
    return rows


def attribute_paywalls(rows, now_ms, rnd):
    """Purchases of the last 30 days in Galaxy Miner partly come from the shop paywall test."""
    for r in rows:
        if r["app_id"].startswith("com.example.galaxyminer") and r["created_at_micros"] > (now_ms - 30 * DAY_MS) * 1000 and r["reason"] == "purchase" and rnd.random() < 0.6:
            v = rnd.choice(["a", "b"])
            r.update(paywall_placement="shop", paywall_variant_id=v, paywall_test_id="shop-ab-1", paywall_id="shop_gems" if v == "a" else "shop_vip_first")


# --- Setup check reports ---------------------------------------------------------------------

OK_TEXT = {
    "google_key": "Play Developer API answers for this package (service account has access)",
    "apple_key": "App Store Server API answers for this bundle (In-App Purchase key)",
    "amazon_api": "Receipt Verification Service accepts the shared secret",
    "stripe_key": "Live key, account charges enabled",
    "stripe_api": "Prices of all subscription and one-time products found",
    "stripe_webhook_secret": "Last signed webhook 25 minutes ago",
    "notifications": "Last notification 38 minutes ago",
    "delivery_identity": "The billing identity may call the game reducers",
    "delivery_game": "grant_purchase and revoke_purchase exist (args v1)",
    "delivery_webhook": "Last delivery answered 200 in 84 ms",
}
PENDING_APP = "com.example.wordgarden"


def green(report):
    """The offline check of the made-up setup fails (no store keys); the demo shows healthy apps."""
    for c in report.get("checks", []):
        if c["id"].startswith("integration:"):
            continue  # real verdicts from the integration health written below
        if c["id"] in OK_TEXT:
            c["detail"] = OK_TEXT[c["id"]]
        c["level"], c["fix"] = "ok", ""
        if c["id"] == "ledger":
            pending = report.get("app_id") == PENDING_APP
            c["level"] = "warn" if pending else "ok"
            c["detail"] = "Purchases waiting for a delivery retry: the webhook answers 503" if pending else "Nothing waiting"
    counts = {}
    for c in report.get("checks", []):
        counts[c["level"]] = counts.get(c["level"], 0) + 1
    report["counts"], report["summary"] = counts, "warn" if counts.get("warn") else "ok"
    report.setdefault("urls", {})["billing_identity"] = "c2007f3e9a51" + "4be8d2c6" * 6 + "5be1"
    return report


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("wasm")
    ap.add_argument("--server", default="http://127.0.0.1:3013")
    ap.add_argument("--db", default="billing-demo")
    ap.add_argument("--out", default=OUT)
    a = ap.parse_args()
    rnd = random.Random(20261110)
    srv = Server(a.server, "")
    s, out = srv.req("POST", "/v1/identity", auth=False)
    srv.token = json.loads(out)["token"]
    srv.db = f"{a.db}-{int(time.time())}"
    s, out = srv.req("PUT", f"/v1/database/{srv.db}", open(a.wasm, "rb").read(), "application/wasm")
    if s >= 300:
        raise SystemExit(f"publish: {s} {out[:300]}")
    print("published", srv.db)
    srv.sql("UPDATE admin SET name = 'Alex Morgan' WHERE protected = true")

    now_ms = int(time.time() * 1000)
    srv.call("admin_set_setting", "public_base_url", f"https://stdb.example.com/v1/database/{a.db}/route")
    srv.call("admin_add_spacetime_host", "stdb.example.com")
    for scope, name in [("default", "google_service_account_json"), ("default", "apple_issuer_id"), ("default", "apple_key_id"),
                        ("default", "apple_private_key_p8"), ("default", "amazon_shared_secret"), ("default", "stripe_secret_key"),
                        ("default", "stripe_webhook_secret")]:
        srv.call("admin_set_credential", scope, name, "demo-%x" % rnd.getrandbits(64))
    for game, d in GAMES.items():
        for app, platform, store, st in d["builds"]:
            delivery = {"delivery": "spacetimedb", "spacetime_url": "https://stdb.example.com", "spacetime_db": game,
                        "grant_reducer": "grant_purchase", "revoke_reducer": "revoke_purchase", "args_format": "v1"} if game == "galaxy-miner" \
                else {"delivery": "webhook", "webhook_url": "https://api.example.com/billing/events"}
            srv.call("admin_upsert_game_app", json.dumps({"app_id": app, "platform": platform, "store": store, "game_key": game,
                                                          "account": "", "allow_sandbox": True, "enabled": True, **delivery}))
            srv.call("admin_set_credential", app, "spacetime_token" if game == "galaxy-miner" else "webhook_secret", "demo-%x" % rnd.getrandbits(64))
            if st == "google":
                srv.call("admin_set_credential", app, "google_rtdn_secret", "%032x" % rnd.getrandbits(128))
            srv.call("admin_upsert_products", json.dumps([
                {"app_id": app, "product_id": p, "kind": k, "default_price_micros": int(usd * 1e6), "currency": "USD"} for p, k, usd, _ in d["products"]]))
    for app in ("com.example.galaxyminer", "com.example.galaxyminer.ios"):
        srv.call("admin_upsert_integration", json.dumps({"app_id": app, "kind": "ga4", "enabled": True,
                                                          "settings": {"firebase_app_id": "1:123456789012:android:0a1b2c3d4e5f6a7b" if "ios" not in app else "1:123456789012:ios:0a1b2c3d4e5f6a7b"}}))
        srv.call("admin_set_credential", app, "ga4_api_secret", "demo-%x" % rnd.getrandbits(64))
        srv.call("admin_upsert_integration", json.dumps({"app_id": app, "kind": "appsflyer", "enabled": True,
                                                          "settings": {"app_id": "com.example.galaxyminer" if "ios" not in app else "id1234567890"}}))
        srv.call("admin_set_credential", app, "appsflyer_s2s_key", "demo-%x" % rnd.getrandbits(64))

    def integrate(app, kind, settings, secret):
        srv.call("admin_upsert_integration", json.dumps({"app_id": app, "kind": kind, "enabled": True, "settings": settings}))
        srv.call("admin_set_credential", app, secret, "demo-%x" % rnd.getrandbits(64))

    integrate("com.example.galaxyminer", "tenjin", {}, "tenjin_sdk_key")
    integrate("com.example.galaxyminer.amazon", "adjust", {"app_token": "a1b2c3d4e5f6", "event_tokens": {"purchase": "x7y8z9"}}, "adjust_s2s_token")
    for app in ("com.example.galaxyminer", "com.example.galaxyminer.ios", "com.example.galaxyminer.amazon",
                "com.example.wordgarden", "com.example.wordgarden.ios", "com.example.wordgarden.web"):
        integrate(app, "webhook", {"url": "https://api.example.com/billing/analytics"}, "integration_webhook_secret")
    for app in ("com.example.galaxyminer", "com.example.galaxyminer.ios", "com.example.galaxyminer.amazon"):
        integrate(app, "gcs", {"bucket": "galaxy-miner-events", "folder": "purchases"}, "gcs_service_account_json")
    for app in ("com.example.wordgarden", "com.example.wordgarden.ios"):
        integrate(app, "ga4", {"firebase_app_id": "1:210987654321:%s:9f8e7d6c5b4a3f2e" % ("ios" if "ios" in app else "android")}, "ga4_api_secret")
    srv.call("admin_upsert_paywall", json.dumps({"game_key": "galaxy-miner", "paywall_id": "shop_gems", "name": "Gems first", "product_ids": ["gems_small", "gems_medium", "gems_large", "vip_monthly"]}))
    srv.call("admin_upsert_paywall", json.dumps({"game_key": "galaxy-miner", "paywall_id": "shop_vip_first", "name": "VIP first",
                                                  "product_ids": ["vip_monthly", "vip_yearly", "gems_medium"], "remote_config": {"title": "Become a VIP", "highlight": "vip_yearly"}}))
    srv.call("admin_upsert_placement", json.dumps({"game_key": "galaxy-miner", "placement_id": "shop", "enabled": True,
                                                    "variants": [{"variant_id": "a", "paywall_id": "shop_gems", "weight": 1}, {"variant_id": "b", "paywall_id": "shop_vip_first", "weight": 1}]}))
    srv.call("admin_start_paywall_test", "galaxy-miner", "shop", "shop-ab-1")

    srv.call("admin_set_app_name", "galaxy-miner", "Galaxy Miner")
    srv.call("admin_set_app_name", "word-garden", "Word Garden")

    def coworker(name, role):
        s2 = Server(a.server, srv.db)
        _, out2 = s2.req("POST", "/v1/identity", auth=False)
        ident = json.loads(out2)
        s2.token = ident["token"]
        srv.call("admin_upsert_admin", json.dumps({"identity": ident["identity"], "name": name, "role": role}))
        return s2

    anna = coworker("Anna Petrova", "editor")
    max_ = coworker("Max Keller", "support")
    coworker("Lena Hoffmann", "viewer")
    anna.call("admin_upsert_paywall", json.dumps({"game_key": "galaxy-miner", "paywall_id": "shop_vip_first", "name": "VIP first",
                                                   "product_ids": ["vip_monthly", "vip_yearly", "gems_medium"],
                                                   "remote_config": {"title": "Become a VIP", "highlight": "vip_yearly", "badge": "Best value"}}))
    anna.call("admin_set_fx_override", "TRY", 34.5)

    rows = make_purchases(now_ms, rnd)
    attribute_paywalls(rows, now_ms, rnd)
    for i in range(0, len(rows), 500):
        srv.call("admin_import_purchases", json.dumps(rows[i:i + 500]))
    print("imported", len(rows), "purchases of", len({r["user_id"] for r in rows}), "players")
    player = rows[-1]["user_id"]
    max_.call("admin_grant_manual", json.dumps({"app_id": "com.example.galaxyminer", "user_id": player, "product_id": "gems_medium",
                                                 "note": "compensation: lost gems after a crash"}))
    today = now_ms // DAY_MS
    for d in range(today - 59, today + 1):
        for v in ("a", "b"):
            views = int(rnd.gauss(70 + (d - today + 60) * 0.8, 8))
            key = f"{d}|galaxy-miner/shop|shop-ab-1|{v}"
            srv.sql(f"INSERT INTO paywall_daily (key, placement_key, day, test_id, variant_id, paywall_id, views) VALUES "
                    f"('{key}', 'galaxy-miner/shop', {d}, 'shop-ab-1', '{v}', '{'shop_gems' if v == 'a' else 'shop_vip_first'}', {views})")
    # What happened to the integration events: sent a few minutes ago; the Amazon build's players
    # send no Adjust device id, so its events are skipped.
    for i in srv.sql("SELECT app_id, kind FROM integration"):
        app, kind = i["app_id"], i["kind"]
        sent = (now_ms - rnd.randrange(2, 40) * 60_000) * 1000
        skipped, reason = 0, ""
        if (app, kind) == ("com.example.galaxyminer.amazon", "adjust"):
            sent, skipped, reason = (now_ms - 3 * DAY_MS) * 1000, (now_ms - 25 * 60_000) * 1000, "client did not send adjust_id or a device id"
        srv.sql("INSERT INTO integration_health (key, app_id, kind, last_sent_at_micros, last_failed_at_micros, last_error, "
                "last_failure_final, last_skipped_at_micros, last_skip_reason) VALUES "
                f"('{app}/{kind}', '{app}', '{kind}', {sent}, 0, '', false, {skipped}, '{reason}')")
    # Start the revenue rebuild now instead of 5 minutes after the import.
    srv.sql("UPDATE server_state SET value = '0' WHERE key = 'last_import_micros'")
    srv.call("admin_ensure_worker")
    print("waiting for the revenue rebuild", end="", flush=True)
    for _ in range(240):
        st = srv.proc("admin_revenue_stats", json.dumps({"summary": True}))
        if not st.get("rebuilding") and st["totals"].get("purchases", 0) > 0:
            break
        print(".", end="", flush=True)
        time.sleep(2)
    print()

    # --- Snapshot ------------------------------------------------------------------------------
    apps = [r["app_id"] for r in srv.sql("SELECT app_id FROM game_app")]
    checks = {app: green(srv.proc("admin_check_app", app, json.dumps({"online": False, "apple_test": False}))) for app in apps}
    tables = {}
    for t in ["game_app", "product", "purchase", "purchase_log", "integration", "app_check", "admin", "retry_schedule",
              "alert_channel", "alert_state", "alert_log", "spacetime_host", "server_state", "paywall", "placement", "paywall_test", "integration_health",
              "app_profile", "admin_audit"]:
        tables[t] = srv.sql(f"SELECT * FROM {t}")
    tables["credential"] = srv.sql("SELECT key FROM credential")  # names only, never values
    # The purchase history is only opened for recent purchases: keep 90 days of it.
    recent = {r["id"] for r in tables["purchase"] if int(r["created_at_micros"]) > (now_ms - 90 * DAY_MS) * 1000}
    tables["purchase_log"] = [r for r in tables["purchase_log"] if r["purchase_id"] in recent]
    for r in tables["app_check"]:
        if r["app_id"] in checks:
            r["report_json"] = json.dumps(checks[r["app_id"]])
            r["summary"] = checks[r["app_id"]]["summary"]

    procs = {"admin_overview": srv.proc("admin_overview"), "whoami": srv.proc("whoami") if False else None}
    day = dt.datetime.fromtimestamp(now_ms / 1000, dt.timezone.utc).date()
    iso = lambda d: d.isoformat()
    revenue = {}

    def rkey(q):
        return "|".join([q.get("from", ""), q.get("to", ""), q.get("game_key", ""), "s" if q.get("summary") else "f"])

    for days in (7, 30, 90, 365):
        frm = day - dt.timedelta(days=days - 1)
        prev_to, prev_from = frm - dt.timedelta(days=1), frm - dt.timedelta(days=days)
        for game in ["", *GAMES]:
            for q in ({"from": iso(frm), "to": iso(day), "game_key": game, "summary": False},
                      {"from": iso(frm), "to": iso(day), "game_key": game, "summary": True},
                      {"from": iso(prev_from), "to": iso(prev_to), "game_key": game, "summary": True}):
                revenue[rkey(q)] = srv.proc("admin_revenue_stats", json.dumps({**q, "include_sandbox": False}))
    profiles = {u: srv.proc("admin_user_profile", u) for u in sorted({r["user_id"] for r in tables["purchase"]})}
    paywall_results = {}
    for test in ("shop-ab-1", ""):
        paywall_results[f"galaxy-miner|shop|{test}"] = srv.proc("admin_paywall_results", json.dumps(
            {"game_key": "galaxy-miner", "placement_id": "shop", "test_id": test, "include_sandbox": False}))
    previews = {f"{app}|shop": srv.proc("admin_preview_paywall", app, "shop", "player-demo") for app in apps if "galaxyminer" in app}
    snapshot = {
        "generated_at_ms": now_ms,
        "version": json.loads(srv.req("GET", f"/v1/database/{srv.db}/route/healthz", auth=False)[1])["version"],
        "identity": next((r["identity"].replace("0x", "") for r in tables["admin"] if r["protected"]), ""),
        "tables": tables,
        "procs": {"admin_overview": procs["admin_overview"], "revenue": revenue, "profiles": profiles,
                  "paywall_results": paywall_results, "previews": previews, "checks": checks},
    }
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    with open(a.out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(snapshot, f, separators=(",", ":"), sort_keys=True)
        f.write("\n")
    print("wrote", a.out, os.path.getsize(a.out) // 1024, "KB")


if __name__ == "__main__":
    main()
