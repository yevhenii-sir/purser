"""An in-memory stand-in for the purser database behind `stdb` (no network): the admin procedures and
reducers the scripts call, recorded in `calls`. Use as a context manager:

    with FakeServer() as s:
        s.add_app("com.example.mygame", "android")
        bulk_setup.main([...])
"""
import json
import os
import sys
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import stdb  # noqa: E402

BASE = "http://localhost:3000"
DB = "billing-test"
# What the reducers store when a field is left out (`GameAppInput`, `ProductInput`).
APP_DEFAULTS = {"game_key": "", "account": "default", "spacetime_url": "", "spacetime_db": "", "grant_reducer": "",
                "revoke_reducer": "", "webhook_url": "", "allow_sandbox": True, "enabled": True,
                "client_consumes": False, "apple_refund_mode": "", "sns_topic_arn": "", "client_auth": "open"}
DEFAULT_STORE = {"android": "google", "ios": "apple", "amazon": "amazon", "steam": "steam"}
PRODUCT_DEFAULTS = {"price_micros": 0, "currency": "", "title": "", "description": "", "period": "",
                    "apple_group": "", "catalog_id": "", "access": ""}


class FakeServer:
    def __init__(self):
        self.apps = {}
        self.products = {}
        self.catalogs = {}  # app_id -> admin_list_store_products answer, or {"error": ...}
        self.reports = {}  # app_id -> admin_check_app answer
        self.refuse_products = set()  # product ids admin_upsert_products refuses
        self.calls = []
        self.owner = True
        self._patches = []

    # -- state ----------------------------------------------------------------------------------

    def add_app(self, app_id, platform, **fields):
        row = dict(APP_DEFAULTS, app_id=app_id, platform=platform, store=DEFAULT_STORE.get(platform, ""),
                   delivery="none")
        row.update(fields)
        row["game_key"] = row["game_key"] or app_id
        self.apps[app_id] = row

    def add_product(self, app_id, product_id, kind, **fields):
        self.products[f"{app_id}:{product_id}"] = dict(PRODUCT_DEFAULTS, key=f"{app_id}:{product_id}", app_id=app_id,
                                                       product_id=product_id, kind=kind, **fields)

    def writes(self):
        return [c for c in self.calls if c[0].startswith("admin_upsert") or c[0] == "admin_setup_google"]

    # -- stdb -----------------------------------------------------------------------------------

    def __enter__(self):
        for name, fn in (("call_raw", self.call_raw), ("select_raw", self.select_raw),
                         ("server_url", lambda nick: BASE), ("token", lambda: "test-token")):
            p = mock.patch.object(stdb, name, side_effect=fn)
            p.start()
            self._patches.append(p)
        return self

    def __exit__(self, *exc):
        for p in self._patches:
            p.stop()
        return False

    def select_raw(self, base, db, tok, query):
        self.calls.append(("sql", query))
        if not self.owner:
            raise stdb.StdbError("no such table", 400, "no such table: `game_app`")
        assert query.strip() == "SELECT * FROM game_app", query
        return [dict(r) for r in self.apps.values()]

    def call_raw(self, base, db, tok, name, *args):
        self.calls.append((name,) + args)
        handler = getattr(self, name, None)
        if handler is None:
            raise AssertionError(f"unexpected call {name}")
        return handler(*args)

    # -- procedures and reducers ----------------------------------------------------------------

    def admin_select(self, query_json):
        q = json.loads(query_json)
        rows = {"game_app": self.apps, "product": self.products, "server_state": {}}[q["table"]].values()
        w = q.get("where")
        if w:
            values = w["equals"] if isinstance(w["equals"], list) else [w["equals"]]
            assert len(values) <= 500
            rows = [r for r in rows if r[w["column"]] in values]
        return [dict(r) for r in rows]

    def admin_upsert_game_app(self, text):
        i = json.loads(text)
        if i.get("delivery") not in ("none", "spacetimedb", "webhook"):
            raise stdb.StdbError("HTTP 530", 530, "[ERR_BAD_APP] delivery must be spacetimedb, webhook or none")
        row = dict(APP_DEFAULTS)
        row.update(self.apps.get(i["app_id"], {}))
        row.update(i)
        row["store"] = i.get("store") or DEFAULT_STORE[i["platform"]]
        row["game_key"] = row["game_key"] or row["app_id"]
        self.apps[i["app_id"]] = row
        return None

    def admin_upsert_products(self, text):
        items = json.loads(text)
        new = {}
        for i in items:
            if i["product_id"] in self.refuse_products:
                raise stdb.StdbError("HTTP 530", 530, f"[ERR_BAD_PRODUCT] {i['product_id']}: refused")
            if i["app_id"] not in self.apps:
                raise stdb.StdbError("HTTP 530", 530, f"[ERR_BAD_PRODUCT] unknown app_id {i['app_id']}")
            key = f"{i['app_id']}:{i['product_id']}"
            row = dict(PRODUCT_DEFAULTS, key=key, **i)
            row["currency"] = row["currency"].upper()
            if row["kind"] != "subscription":
                row["period"] = row["apple_group"] = ""
            if row["catalog_id"] == row["product_id"]:
                row["catalog_id"] = ""
            new[key] = row
        self.products.update(new)  # one transaction: all or nothing
        return None

    def admin_list_store_products(self, app_id):
        return self.catalogs.get(app_id, {"error": f"no catalog for {app_id}"})

    def admin_check_app(self, app_id, options_json):
        return self.reports.get(app_id) or {
            "app_id": app_id, "summary": "ok", "counts": {"ok": 3, "warn": 0, "fail": 0}, "checks": [], "urls": {}}

    def admin_setup_google(self, text):
        req = json.loads(text)
        if not 1 <= len(req["app_ids"]) <= 20:
            return {"error": "name 1 to 20 Google Play builds"}
        accounts = {self.apps[i]["account"] for i in req["app_ids"]}
        if len(accounts) > 1 and not req.get("scope"):
            return {"error": "the builds are on different developer accounts: name the scope"}
        return {"ok": True, "dry_run": req.get("dry_run", False), "service_account": "purser@example.iam",
                "scope": req.get("scope") or accounts.pop(), "topic": f"projects/{req['project']}/topics/{req['topic']}",
                "steps": [{"step": f"subscription {i}", "action": "created", "detail": ""} for i in req["app_ids"]],
                "manual": ["Play Console: set the topic"]}
