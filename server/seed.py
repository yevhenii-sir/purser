"""Uploads billing configuration to the purser database via admin reducers, runs the setup
check, exports the configuration back to files and walks through a first setup.

Usage (from server/, after `spacetime login` as the database owner, or with PURSER_TOKEN set):
    python seed.py                      # reads ./secrets/, target from spacetime.json
    python seed.py --dir seed.example   # dry config without secrets
    python seed.py --server local --database billing-local
    python seed.py --check              # upload, then check every app in apps.json
    python seed.py --check-only         # only the check (exit code 1 if anything fails)
    python seed.py --check-only --offline --json
    python seed.py --check-all          # every enabled build of the database: a summary table
                                        # (--jobs 8 checks 8 at a time; exit code 1 if any failed)
    python seed.py --export backup      # write the database's configuration to ./backup/ (no secrets)
    python seed.py --init               # guided first setup: asks questions, writes ./secrets/, uploads
    python seed.py --server-key mygame --key-scopes read,grant   # a key for the game's backend (/server/*)

Files in --dir (all optional), loaded in this order:
    spacetime_hosts.json  array of self-hosted SpacetimeDB hosts -> admin_add_spacetime_host
                      (owner only; loaded first because apps on those hosts are validated against it)
    apps.json         array of builds           -> admin_upsert_game_app
                      {store_app_id, platform, store?, game_key, account, delivery, ...}: store_app_id
                      is the build's id in its store (package name, bundle id; any unique id for
                      stores without one) and an empty store is the platform's default. The server
                      keys the build `{store}:{store_app_id}` (google:com.example.mygame): that key is
                      the `app_id` of every other file and the scope of the build's own secrets.
    app_names.json    {game_key: display name}  -> admin_set_app_name (the admin page's app names)
    products.json     array of products         -> admin_upsert_products
    steam.json        {"builds": [{app_id, steam_app_id?, sandbox?}]  -> admin_set_steam_build,
                       "items": [{app_id, product_id, item_id?, description?, prices?, period?,
                                  frequency?}]                        -> admin_upsert_steam_items}
    credentials.json  [{scope, name, value | value_file}] -> admin_set_credential
                      value_file is relative to --dir; keep ./secrets/ out of git.
    integrations.json array of {app_id, kind, enabled, settings} -> admin_upsert_integration
    paywalls.json     array of {game_key, paywall_id, name, product_ids, remote_config}
                                                -> admin_upsert_paywall
    placements.json   array of {game_key, placement_id, variants: [{variant_id, paywall_id, weight}],
                      enabled}                  -> admin_upsert_placement (A/B tests are started in
                      the admin page; while one runs only `enabled` may change)
    alerts.json       {"channels": [{name?, kind, enabled, settings, secret?, game_keys?, rules?}]
                                                -> admin_upsert_alert_channel,
                       "rules": {...}}          -> admin_set_alert_rules
                      `name` (unique, defaults to the kind) finds the channel to update; several
                      channels of a kind need names. `game_keys` / `rules` limit a channel to some
                      apps / rules (empty = all). A channel without `secret` keeps the stored one
                      (credential alert-<id>/alert_*); an enabled channel without any is skipped.
    settings.json     {public_base_url, store_commission_percent, store_commission_by_app,
                       store_commission_after_year_percent}  -> admin_set_setting
                      (store_commission_by_app: {build key: percent})
Values starting with `<` are placeholders and are skipped.

--export writes the same files from the database (owner only: it reads the tables with /sql).
Secret values are never read: credentials.json lists the keys that are set, with "<set me>".
"""
import argparse
import concurrent.futures
import getpass
import hashlib
import json
import os
import pathlib
import secrets
import sys

import stdb

ROOT = pathlib.Path(__file__).resolve().parent
REGISTRY = ROOT / "admin-ui" / "src" / "lib" / "registry.json"
MARK = {"ok": "ok  ", "warn": "WARN", "fail": "FAIL", "skip": "skip", "info": "info"}
# `settings::ADMIN_KEYS` in spacetimedb/src/settings.rs.
SETTING_KEYS = ["public_base_url", "store_commission_percent", "store_commission_by_app",
                "store_commission_after_year_percent"]
# `alerts::RULES_KEY`.
ALERT_RULES_KEY = "alert_rules"
PLACEHOLDER = "<set me>"
# `admin_upsert_alert_channel` in spacetimedb/src/alerts.rs: an enabled channel without a stored secret.
ALERT_SECRET_MISSING_CODE = "[ERR_ALERT_SECRET_MISSING]"
# `alerts::secret_for`: the secret of a channel kind, stored as `alert-{id}/<name>` (`alerts::secret_scope`).
ALERT_SECRETS = {"telegram": "alert_telegram_bot_token", "slack": "alert_slack_webhook_url",
                 "discord": "alert_discord_webhook_url", "webhook": "alert_webhook_secret"}
# Store keys that are identifiers, not secrets: --init echoes them while they are typed.
PLAIN_KEYS = {"apple_issuer_id", "apple_key_id", "apple_connect_key_id", "xsolla_project_id"}


# ---------------------------------------------------------------------------------------------
# Database access
# ---------------------------------------------------------------------------------------------

def conn(a):
    """(base URL, token) of the target, resolved once."""
    if not hasattr(a, "_conn"):
        a._conn = (stdb.server_url(a.server), stdb.token())
    return a._conn


def try_call(a, reducer, *args):
    """Calls a reducer (every argument is a string); returns the error text, or None on success."""
    base, tok = conn(a)
    try:
        stdb.call_raw(base, a.database, tok, reducer, *args)
    except stdb.StdbError as e:
        return (e.body or str(e)).strip()
    return None


def call(a, reducer, *args):
    err = try_call(a, reducer, *args)
    if err is not None:
        sys.exit(f"{reducer} failed:\n{err}")


def proc(a, name, *args):
    """Calls an admin procedure; exits on `{"error"}`."""
    base, tok = conn(a)
    v = stdb.call(base, a.database, tok, name, *args)
    if isinstance(v, dict) and "error" in v:
        sys.exit(f"{name} failed: {v['error']}")
    return v


def sql(a, query):
    base, tok = conn(a)
    return stdb.select(base, a.database, tok, query)


def pmap(fn, items, jobs):
    """`fn` over `items`, `jobs` at a time, results in the order of `items`. Call `conn(a)` first:
    the threads share the resolved connection."""
    items = list(items)
    if jobs <= 1 or len(items) <= 1:
        return [fn(i) for i in items]
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        return list(pool.map(fn, items))


# ---------------------------------------------------------------------------------------------
# Upload
# ---------------------------------------------------------------------------------------------

def load(path):
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else None


def has_placeholder(v):
    if isinstance(v, str):
        return v.startswith("<")
    if isinstance(v, dict):
        return any(has_placeholder(x) for x in v.values())
    if isinstance(v, list):
        return any(has_placeholder(x) for x in v)
    return False


def alert_secret_missing(err, name):
    """The reducer refused an enabled channel because no secret is stored yet."""
    if ALERT_SECRET_MISSING_CODE in err:
        return True
    text = err.strip()
    return text.startswith("[ERR_BAD_ALERT]") and f"] {name}: set " in text and text.endswith(" first")


def is_alert_scope(scope):
    """The credential scope of an alert channel's secret (`alert-<id>`)."""
    return scope.startswith("alert-") and scope[len("alert-"):].isdigit()


def alert_channel_label(ch):
    """The name of a channel of alerts.json (defaults to the kind) and how to print it, with its filters."""
    name = (ch.get("name") or "").strip() or ch["kind"]
    label = name if name == ch["kind"] else f"{name} ({ch['kind']})"
    filters = []
    if ch.get("game_keys"):
        filters.append("apps " + ",".join(ch["game_keys"]))
    if ch.get("rules"):
        filters.append("rules " + ",".join(ch["rules"]))
    return name, label + (f" [{'; '.join(filters)}]" if filters else "")


def app_key(app, where="apps.json"):
    """The key of a build of apps.json, `{store}:{store_app_id}` as the server makes it; exits on a
    platform or store the server would refuse."""
    try:
        return stdb.build_key(app.get("platform", ""), app.get("store_app_id", ""), app.get("store", ""))
    except ValueError as e:
        sys.exit(f"{where}: {app.get('store_app_id') or json.dumps(app)}: {e}")


def seed(a, d):
    for host in load(d / "spacetime_hosts.json") or []:
        if has_placeholder(host):
            continue
        call(a, "admin_add_spacetime_host", host)
        print(f"host     {host}")

    for app in load(d / "apps.json") or []:
        key = app_key(app)
        call(a, "admin_upsert_game_app", json.dumps(app))
        print(f"app      {key}")

    for key, name in (load(d / "app_names.json") or {}).items():
        if has_placeholder(name):
            continue
        call(a, "admin_set_app_name", key, name)
        print(f"name     {key}: {name}")

    products = load(d / "products.json") or []
    if products:
        call(a, "admin_upsert_products", json.dumps(products))
        print(f"products {len(products)}")

    steam = load(d / "steam.json") or {}
    for b in steam.get("builds") or []:
        call(a, "admin_set_steam_build", json.dumps(b))
        print(f"steam    {b['app_id']} sandbox={b.get('sandbox', False)}")
    if steam.get("items"):
        call(a, "admin_upsert_steam_items", json.dumps(steam["items"]))
        print(f"steam    {len(steam['items'])} item(s)")

    for c in load(d / "credentials.json") or []:
        if "value_file" in c:
            f = d / c["value_file"]
            if not f.exists():
                print(f"skip     {c['scope']}/{c['name']} ({c['value_file']} not found)")
                continue
            value = f.read_text(encoding="utf-8")
        else:
            value = c["value"]
        if value.startswith("<") or not value.strip():
            print(f"skip     {c['scope']}/{c['name']} (placeholder)")
            continue
        call(a, "admin_set_credential", c["scope"], c["name"], value)
        print(f"secret   {c['scope']}/{c['name']}")

    for i in load(d / "integrations.json") or []:
        if has_placeholder(i.get("settings") or {}):
            print(f"skip     integration {i['app_id']}/{i['kind']} (placeholder)")
            continue
        call(a, "admin_upsert_integration", json.dumps(i))
        print(f"integr.  {i['app_id']}/{i['kind']} enabled={i.get('enabled', True)}")

    for p in load(d / "paywalls.json") or []:
        if has_placeholder(p):
            print(f"skip     paywall {p.get('game_key')}/{p.get('paywall_id')} (placeholder)")
            continue
        call(a, "admin_upsert_paywall", json.dumps(p))
        print(f"paywall  {p['game_key']}/{p['paywall_id']} ({len(p.get('product_ids') or [])} products)")

    for p in load(d / "placements.json") or []:
        if has_placeholder(p):
            print(f"skip     placement {p.get('game_key')}/{p.get('placement_id')} (placeholder)")
            continue
        call(a, "admin_upsert_placement", json.dumps(p))
        print(f"place.   {p['game_key']}/{p['placement_id']} enabled={p.get('enabled', True)}")

    alerts = load(d / "alerts.json") or {}
    for ch in alerts.get("channels") or []:
        ch = dict(ch)
        # Channels are found by name: an id of another database means nothing here.
        ch.pop("id", None)
        name, label = alert_channel_label(ch)
        if has_placeholder(ch.get("secret", "")):
            ch.pop("secret")
        if has_placeholder(ch.get("settings") or {}):
            print(f"skip     alert channel {label} (placeholder)")
            continue
        err = try_call(a, "admin_upsert_alert_channel", json.dumps(ch))
        if err and not ch.get("secret") and alert_secret_missing(err, name):
            print(f"skip     alert channel {label} ({err.split('] ', 1)[-1]})")
            continue
        if err:
            sys.exit(f"admin_upsert_alert_channel failed:\n{err}")
        print(f"alert    {label} enabled={ch.get('enabled', True)}")
    if alerts.get("rules") is not None:
        call(a, "admin_set_alert_rules", json.dumps(alerts["rules"]))
        print("alert    rules")

    for key, value in (load(d / "settings.json") or {}).items():
        if key not in SETTING_KEYS:
            sys.exit(f"settings.json: unknown setting '{key}' (known: {', '.join(SETTING_KEYS)})")
        if has_placeholder(value):
            print(f"skip     setting {key} (placeholder)")
            continue
        text = value if isinstance(value, str) else json.dumps(value)
        call(a, "admin_set_setting", key, text)
        print(f"setting  {key}")


# ---------------------------------------------------------------------------------------------
# Setup check
# ---------------------------------------------------------------------------------------------


# Optional product fields an export keeps (the shared catalog id, access levels, store listing).
PRODUCT_EXTRAS = ("catalog_id", "access", "title", "description", "period", "apple_group")

UNITY_TYPE = {"consumable": "Consumable", "non_consumable": "NonConsumable", "subscription": "Subscription"}


def unity_config(route, builds, products):
    """The Unity inspector's config (same shape as the admin page's "Copy Unity config"): every
    build by its store and its id there, and every product once by its catalog id with the store ids
    that differ and a price. `builds`: {app_id (the key), store, store_app_id}."""
    store_of = {b["app_id"]: b["store"] for b in builds}
    by_id = {}
    for p in products:
        if p["app_id"] not in store_of:
            continue
        cid = p.get("catalog_id") or p["product_id"]
        item = by_id.setdefault(cid, {"id": cid, "type": UNITY_TYPE.get(p["kind"], p["kind"])})
        if p["product_id"] != cid:
            item.setdefault("storeIds", {})[store_of[p["app_id"]]] = p["product_id"]
        if p.get("price_micros") and p.get("currency") and "price" not in item:
            item.update(price=p["price_micros"] / 1e6, currency=p["currency"])
        if p.get("access") and "access" not in item:
            item["access"] = [a for a in p["access"].split(",") if a]
    config = {"serverBaseUrl": route, "builds": [{"store": b["store"], "appId": b["store_app_id"]} for b in builds],
              "products": list(by_id.values())}
    return json.dumps(config, indent=2)

def run_check(a, app_ids, quiet=False):
    """Runs admin_check_app for the builds. Returns (ok, reports, route base)."""
    base, _ = conn(a)
    options = json.dumps({"online": not a.offline, "apple_test": not a.offline})
    reports, ok = [], True
    route = f"{base}/v1/database/{a.database}/route"
    for app_id in app_ids:
        report = proc(a, "admin_check_app", app_id, options)
        reports.append(report)
        ok = ok and report["summary"] != "fail"
        route = report["urls"].get("base") or route
        if quiet:
            continue
        print(f"\n{app_id}: {report['summary'].upper()}")
        for c in report["checks"]:
            print(f"  [{MARK.get(c['level'], c['level'])}] {c['title']}: {c['detail']}")
            if c["fix"] and c["level"] in ("warn", "fail"):
                print(f"         -> {c['fix']}")
        r = report["urls"].get("base") or f"{base}/v1/database/{a.database}/route"
        for name, url in report["urls"].items():
            if url and name in ("notify", "billing_identity"):
                print(f"  {name}: {r + url if url.startswith('/') else url}")
    return ok, reports, route


def check(a, d):
    """Runs admin_check_app for every build in apps.json. Returns True when nothing failed."""
    apps = [app_key(x) for x in load(d / "apps.json") or []]
    if not apps:
        sys.exit(f"no apps in {d / 'apps.json'} to check")
    ok, reports, _ = run_check(a, apps, quiet=a.json)
    if a.json:
        print(json.dumps(reports, indent=2))
    return ok


def list_builds(a):
    """Every build: the owner reads `game_app` with /sql, other admins through `admin_select`."""
    base, tok = conn(a)
    try:
        return stdb.select_raw(base, a.database, tok, "SELECT * FROM game_app")
    except stdb.StdbError:
        v = stdb.call(base, a.database, tok, "admin_select", json.dumps({"table": "game_app"}))
        if isinstance(v, dict) and "error" in v:
            sys.exit(f"admin_select game_app failed: {v['error']}")
        return v or []


def check_one(a, app_id, options):
    """admin_check_app of one build; a failed call is `{"app_id", "error"}` (the other builds go on)."""
    base, tok = conn(a)
    try:
        v = stdb.call_raw(base, a.database, tok, "admin_check_app", app_id, options)
    except stdb.StdbError as e:
        return {"app_id": app_id, "error": (e.body or str(e)).strip()}
    if not isinstance(v, dict):
        return {"app_id": app_id, "error": f"unexpected answer {str(v)[:200]}"}
    if "error" in v:
        return {"app_id": app_id, "error": str(v["error"])}
    return v


def check_table(builds, reports):
    """The summary of --check-all: a line per build (ok / warnings / failures), the totals, then what
    failed. Returns (text, ok)."""
    rows = [("APP", "BUILD", "STORE", "RESULT", "OK", "WARN", "FAIL")]
    failed = []
    for b, r in zip(builds, reports):
        n = r.get("counts") or {}
        result = "ERROR" if "error" in r else str(r.get("summary", "")).upper()
        rows.append((b["game_key"], b["app_id"], b.get("store", ""), result,
                     str(n.get("ok", "")), str(n.get("warn", "")), str(n.get("fail", ""))))
        if "error" in r or r.get("summary") == "fail":
            failed.append(r)
    widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
    lines = ["  ".join(c.ljust(w) for c, w in zip(row, widths)).rstrip() for row in rows]
    total = {s: sum(1 for r in reports if r.get("summary") == s) for s in ("ok", "warn", "fail")}
    errors = sum(1 for r in reports if "error" in r)
    lines.append(f"\n{len(reports)} build(s): {total['ok']} ok, {total['warn']} with warnings, {total['fail']} failed"
                 + (f", {errors} not checked" if errors else ""))
    for r in failed:
        lines.append(f"\n{r['app_id']}:")
        if "error" in r:
            lines.append(f"  [ERROR] {r['error']}")
            continue
        for c in r.get("checks") or []:
            if c.get("level") == "fail":
                lines.append(f"  [FAIL] {c['title']}: {c['detail']}")
                if c.get("fix"):
                    lines.append(f"         -> {c['fix']}")
    return "\n".join(lines), not failed


def check_all(a):
    """Runs admin_check_app for every enabled build of the database. Returns True when nothing failed."""
    builds = sorted(list_builds(a), key=lambda b: (b["game_key"], b["app_id"]))
    disabled = [b for b in builds if not b.get("enabled", True)]
    builds = [b for b in builds if b.get("enabled", True)]
    if not builds:
        sys.exit("no enabled builds in the database")
    print(f"checking {len(builds)} build(s), {a.jobs} at a time"
          + (f"; {len(disabled)} disabled build(s) skipped" if disabled else ""), file=sys.stderr)
    options = json.dumps({"online": not a.offline, "apple_test": not a.offline})
    reports = pmap(lambda b: check_one(a, b["app_id"], options), builds, a.jobs)
    text, ok = check_table(builds, reports)
    print(json.dumps(reports, indent=2) if a.json else text)
    return ok


# ---------------------------------------------------------------------------------------------
# Export
# ---------------------------------------------------------------------------------------------

def parse_json(text, default):
    try:
        return json.loads(text) if text and text.strip() else default
    except ValueError:
        return default


def export(a, d, force):
    if d.exists() and any(d.iterdir()) and not force:
        sys.exit(f"{d} is not empty: pass --force to overwrite its files")
    d.mkdir(parents=True, exist_ok=True)
    files = {}

    hosts = sorted(r["host"] for r in sql(a, "SELECT host FROM spacetime_host"))
    if hosts:
        files["spacetime_hosts.json"] = hosts

    # The key (`app_id`) is the server's: apps.json names a build by its store and its id there.
    apps = [{k: v for k, v in r.items() if k != "app_id"}
            for r in sorted(sql(a, "SELECT * FROM game_app"), key=lambda r: (r["game_key"], r["app_id"]))]
    if apps:
        files["apps.json"] = apps

    base, tok = conn(a)
    try:
        names = {r["game_key"]: r["display_name"] for r in stdb.select_raw(base, a.database, tok, "SELECT * FROM app_profile")}
    except stdb.StdbError:  # not the owner: /sql is the owner's
        names = {}
    if names:
        files["app_names.json"] = dict(sorted(names.items()))

    # Every field `admin_upsert_products` takes; the optional ones only when set.
    products = [{k: p[k] for k in ("app_id", "product_id", "kind", "price_micros", "currency")}
                | {k: p[k] for k in PRODUCT_EXTRAS if p.get(k)}
                for p in sorted(sql(a, "SELECT * FROM product"), key=lambda p: (p["app_id"], p["product_id"]))]
    if products:
        files["products.json"] = products

    builds = [{"app_id": b["app_id"], "steam_app_id": b["steam_app_id"], "sandbox": b["sandbox"]}
              for b in sorted(sql(a, "SELECT * FROM steam_build"), key=lambda b: b["app_id"])]
    items = []
    for i in sorted(sql(a, "SELECT * FROM steam_item"), key=lambda i: i["key"]):
        item = {"app_id": i["app_id"], "product_id": i["product_id"], "item_id": i["item_id"],
                "description": i["description"], "period": i["period"], "frequency": i["frequency"]}
        prices = parse_json(i["prices_json"], None)
        if prices:
            item["prices"] = prices
        items.append(item)
    if builds or items:
        files["steam.json"] = {"builds": builds, "items": items}

    # Only the key column: secret values never leave the database.
    keys = sorted(r["key"] for r in sql(a, "SELECT key FROM credential"))
    # Secrets of alert channels (alert-<id>/...) go to alerts.json: channel ids differ between databases.
    creds = [{"scope": k.rsplit("/", 1)[0], "name": k.rsplit("/", 1)[1], "value": PLACEHOLDER}
             for k in keys if "/" in k and not is_alert_scope(k.rsplit("/", 1)[0])]
    if creds:
        files["credentials.json"] = creds

    integrations = [{"app_id": i["app_id"], "kind": i["kind"], "enabled": i["enabled"],
                     "settings": parse_json(i["settings_json"], {})}
                    for i in sorted(sql(a, "SELECT * FROM integration"), key=lambda i: i["key"])]
    if integrations:
        files["integrations.json"] = integrations

    paywalls = []
    for p in sorted(sql(a, "SELECT * FROM paywall"), key=lambda p: p["key"]):
        row = {"game_key": p["game_key"], "paywall_id": p["paywall_id"], "name": p["name"],
               "product_ids": parse_json(p["product_ids_json"], [])}
        if p["remote_config_json"].strip():
            row["remote_config"] = parse_json(p["remote_config_json"], None)
        paywalls.append(row)
    if paywalls:
        files["paywalls.json"] = paywalls

    placements = [{"game_key": p["game_key"], "placement_id": p["placement_id"],
                   "variants": parse_json(p["variants_json"], []), "enabled": p["enabled"]}
                  for p in sorted(sql(a, "SELECT * FROM placement"), key=lambda p: p["key"])]
    if placements:
        files["placements.json"] = placements

    state = {r["key"]: r["value"] for r in sql(a, "SELECT key, value FROM server_state")}
    channels = []
    for c in sorted(sql(a, "SELECT * FROM alert_channel"), key=lambda c: (c["name"], c["kind"])):
        ch = {"name": c["name"], "kind": c["kind"], "enabled": c["enabled"], "settings": parse_json(c["settings_json"], {}),
              "game_keys": [k for k in c["game_keys"].split(",") if k.strip()],
              "rules": [r for r in c["rules"].split(",") if r.strip()]}
        # A stored secret is listed, never read: fill it in before uploading the file elsewhere.
        if f"alert-{c['id']}/{ALERT_SECRETS.get(c['kind'], '')}" in keys:
            ch["secret"] = PLACEHOLDER
        channels.append(ch)
    alerts = {}
    if channels:
        alerts["channels"] = channels
    if state.get(ALERT_RULES_KEY):
        alerts["rules"] = parse_json(state[ALERT_RULES_KEY], {})
    if alerts:
        files["alerts.json"] = alerts

    settings = {}
    for k in SETTING_KEYS:
        if state.get(k):
            settings[k] = parse_json(state[k], state[k]) if k == "store_commission_by_app" else state[k]
    if settings:
        files["settings.json"] = settings

    for name, value in files.items():
        (d / name).write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        n = len(value) if isinstance(value, (list, dict)) else 1
        print(f"wrote    {name} ({n})")
    if "credentials.json" in files:
        print(f"         credentials.json lists the keys only: replace \"{PLACEHOLDER}\" with the value or a "
              "\"value_file\" before uploading it elsewhere")
    if any(c.get("secret") == PLACEHOLDER for c in files.get("alerts.json", {}).get("channels", [])):
        print(f"         alerts.json: replace the channels' \"secret\": \"{PLACEHOLDER}\" before uploading it elsewhere "
              "(or remove it to keep the stored one)")


# ---------------------------------------------------------------------------------------------
# First setup (--init)
# ---------------------------------------------------------------------------------------------

def ask(label, default="", required=False):
    while True:
        hint = f" [{default}]" if default else ""
        try:
            v = input(f"{label}{hint}: ").strip()
        except EOFError:
            sys.exit("\naborted")
        v = v or default
        if v or not required:
            return v
        print("  required")


def ask_secret(label):
    """Like `ask`, without echoing the answer."""
    try:
        return getpass.getpass(f"{label}: ").strip()
    except EOFError:
        sys.exit("\naborted")


def ask_yes(label, default=True):
    v = ask(f"{label} ({'Y/n' if default else 'y/N'})").lower()
    return default if not v else v.startswith("y")


def choose(label, options, default):
    while True:
        v = ask(f"{label} ({' / '.join(options)})", default)
        if v in options:
            return v
        print(f"  one of: {', '.join(options)}")


def registry():
    r = json.loads(REGISTRY.read_text(encoding="utf-8"))
    return {s["id"]: s for s in r["stores"]}, {p["id"]: p for p in r["platforms"]}


def write_json(path, value, private=False):
    text = json.dumps(value, indent=2, ensure_ascii=False) + "\n"
    if not private:
        path.write_text(text, encoding="utf-8")
        return
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    if hasattr(os, "fchmod"):
        os.fchmod(fd, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(text)


def init(a):
    stores, platforms = registry()
    print("First setup of the billing database. Answers go to JSON files you can edit and upload again\n"
          "with `python seed.py`; secrets stay in that folder (keep it out of git).\n")
    a.server = ask("SpacetimeDB server (CLI nickname or URL)", a.server, required=True)
    a.database = ask("Billing database name", a.database if a.database and not a.database.startswith("<") else "",
                     required=True)
    print(f"connecting to {a.server}/{a.database} ...")
    sql(a, "SELECT app_id FROM game_app")  # fails early when the database or the owner token is wrong
    cfg_path = ROOT / "spacetime.json"
    if not cfg_path.exists() and ask_yes("Save server and database to server/spacetime.json?"):
        write_json(cfg_path, {"server": a.server, "module-path": "./spacetimedb", "database": a.database})

    d = (ROOT / ask("Folder for the configuration files", a.dir)).resolve()
    if d.exists() and any(d.iterdir()) and not ask_yes(f"{d} is not empty; overwrite its files?", False):
        sys.exit("aborted")
    d.mkdir(parents=True, exist_ok=True)

    print("\nAn app is one game; it has a build per store.")
    game_key = ask("App key (e.g. my-game)", required=True)
    account = ask("Developer account: the secrets profile of the store accounts the builds are published from",
                  "default")
    builds = []
    for platform in ("android", "ios", "amazon"):
        store = stores[platforms[platform]["default_store"]]
        store_app_id = ask(f"{store['name']} build: {store['store_app_id_label']} (empty: none)")
        if store_app_id:
            builds.append({"app_id": stdb.build_key(platform, store_app_id, store["id"]), "store_app_id": store_app_id,
                           "platform": platform, "store": store["id"]})
    if not builds:
        sys.exit("no builds: add Steam or web builds in the admin page or in apps.json")

    print("\nDelivery: how the game learns about purchases.\n"
          "  none        the client grants the reward itself after /verify\n"
          "  spacetimedb the billing module calls a reducer of the game's SpacetimeDB database\n"
          "  webhook     the billing module POSTs a signed JSON to your backend")
    delivery = choose("Delivery", ["none", "spacetimedb", "webhook"], "none")
    target = {"delivery": delivery}
    creds, generated = [], {}
    if delivery == "spacetimedb":
        target.update({
            "spacetime_url": ask("Game database server URL", "https://maincloud.spacetimedb.com", required=True),
            "spacetime_db": ask("Game database name", required=True),
            "grant_reducer": ask("Grant reducer", "billing_grant", required=True),
            "revoke_reducer": ask("Revoke reducer (empty: none)", "billing_revoke"),
        })
        tok = ask_secret("Long-lived token of the identity the game module trusts (empty: set it later)")
        creds.append({"scope": account, "name": "spacetime_token", "value": tok or "<token of the identity trusted by the game module>"})
    elif delivery == "webhook":
        target["webhook_url"] = ask("Webhook URL (https://...)", required=True)
        creds.append({"scope": account, "name": "webhook_secret", "value": secrets.token_hex(32)})

    apps = [{"store_app_id": b["store_app_id"], "platform": b["platform"], "game_key": game_key, "account": account,
             **target, "allow_sandbox": True, "enabled": True} for b in builds]

    asked = set()
    for b in builds:
        store = stores[b["store"]]
        print(f"\n{store['name']} keys for {b['app_id']} (docs/store-setup.md#{store['docs']}):")
        for s in store["secrets"]:
            scope = account if s["account_wide"] else b["app_id"]
            if (scope, s["name"]) in asked:
                continue
            asked.add((scope, s["name"]))
            if s["name"] == store.get("notify_secret"):
                value = secrets.token_hex(32)
                generated.setdefault(b["app_id"], {})[s["name"]] = value
                creds.append({"scope": scope, "name": s["name"], "value": value})
                print(f"  {s['name']}: generated")
                continue
            optional = s["need"] == "optional"
            suffix = " (optional, empty: skip)" if optional else " (empty: set it later)"
            if s["file"]:
                path = ask(f"  {s['label']}: path to the file{suffix}")
                if path:
                    f = pathlib.Path(path.strip('"')).expanduser().resolve()
                    if not f.is_file():
                        print(f"  {f} not found; set it later in credentials.json")
                    else:
                        try:
                            rel = str(f.relative_to(d))
                        except ValueError:
                            rel = str(f)
                        creds.append({"scope": scope, "name": s["name"], "value_file": rel})
                        continue
            else:
                prompt = ask if s["name"] in PLAIN_KEYS else ask_secret
                value = prompt(f"  {s['label']}{suffix}")
                if value:
                    creds.append({"scope": scope, "name": s["name"], "value": value})
                    continue
            if not optional:
                creds.append({"scope": scope, "name": s["name"], "value": f"<{s['label']}>"})

    write_json(d / "apps.json", apps)
    write_json(d / "credentials.json", creds, private=True)
    products_path = d / "products.json"
    if not products_path.exists():
        write_json(products_path, [])
    print(f"\nwrote    {d / 'apps.json'}, credentials.json, products.json\n")
    seed(a, d)

    products = load(products_path) or []
    for b in builds:
        store = stores[b["store"]]
        if not store.get("product_import") or not ask_yes(f"\nImport the {store['name']} catalog of {b['app_id']}?"):
            continue
        base, tok = conn(a)
        res = stdb.call(base, a.database, tok, "admin_list_store_products", b["app_id"])
        if not isinstance(res, dict) or "error" in res:
            print(f"  import failed: {res.get('error') if isinstance(res, dict) else res}")
            print("  (fix the keys, then run the import from the admin page or again with --init)")
            continue
        new = [r for r in res.get("products", []) if r.get("status") == "new"]
        if not new:
            print("  nothing new")
            continue
        for r in new:
            price = f"{r['price_micros'] / 1e6:g} {r['currency']}" if r.get("price_micros") else "no price"
            print(f"  {r['product_id']:<32} {r['kind']:<13} {price}")
        one_time = [r["product_id"] for r in new if r["kind"] != "subscription"]
        non_cons = set()
        if one_time:
            v = ask("  Which one-time products are non-consumable (bought once)? Comma-separated ids, empty: none")
            non_cons = {x.strip() for x in v.split(",") if x.strip()}
        rows = [{"app_id": b["app_id"], "product_id": r["product_id"],
                 "kind": "non_consumable" if r["product_id"] in non_cons else r["kind"],
                 "price_micros": r.get("price_micros", 0), "currency": r.get("currency", "")} for r in new]
        targets = [b["app_id"]] + [o["app_id"] for o in builds if o is not b and not stores[o["store"]].get("product_import")
                                   and ask_yes(f"  Add the same products to {o['app_id']}?")]
        rows = [dict(r, app_id=t) for t in targets for r in rows]
        call(a, "admin_upsert_products", json.dumps(rows))
        keys = {(r["app_id"], r["product_id"]) for r in rows}
        products = [p for p in products if (p["app_id"], p["product_id"]) not in keys] + rows
        write_json(products_path, products)
        print(f"products {len(rows)} imported (saved to products.json)")

    print("\nSetup check:")
    _, reports, route = run_check(a, [b["app_id"] for b in builds])

    print("\nPaste into the store consoles:")
    for rep in reports:
        url = rep["urls"].get("notify") or ""
        if not url:
            continue
        if url.startswith("/"):
            url = route + url
        for name, value in generated.get(rep["app_id"], {}).items():
            url = url.replace(f"<{name}>", value)
        label = rep["urls"].get("notify_label") or "Store notifications URL"
        print(f"  {rep['app_id']}: {label}\n    {url}")
        if "<" in url:
            print("    (replace the <...> part with the secret from credentials.json)")
    if not reports[0]["urls"].get("base"):
        print("  If clients and stores reach the database through another address than this one, set\n"
              "  public_base_url (settings.json or Settings -> Server) and run the check again.")

    print("\nUnity: BillingConfig -> Paste config from admin panel:")
    print(unity_config(route, builds, products))
    try:
        shown = d.relative_to(ROOT)
    except ValueError:
        shown = d
    print(f"\nDone. Edit the files in {d} and run `python seed.py --dir \"{shown}\" --check` to change the setup.")


# ---------------------------------------------------------------------------------------------

SERVER_KEY_SCOPES = ("read", "grant", "revoke")


def new_server_key():
    """A server key as the admin page makes it: `psk_<key_id>_<secret>`, the id public, only the SHA-256 of
    the whole key stored on the server. Returns (key_id, key, key_hash)."""
    key_id = secrets.token_hex(6)
    key = f"psk_{key_id}_{secrets.token_urlsafe(32)}"
    return key_id, key, hashlib.sha256(key.encode()).hexdigest()


def server_key_scopes(text):
    """`read, grant` -> `read,grant`; exits on an unknown or missing scope."""
    scopes = [s.strip() for s in text.split(",") if s.strip()]
    bad = [s for s in scopes if s not in SERVER_KEY_SCOPES]
    if bad or not scopes:
        sys.exit(f"--key-scopes: {', '.join(bad) or 'none given'}; use {', '.join(SERVER_KEY_SCOPES)}")
    return ",".join(dict.fromkeys(scopes))


def add_server_key(a):
    """Makes a key for the game's backend (/server/entitlements, /server/grant, /server/revoke) and prints it
    once: the server keeps only its hash."""
    scopes = server_key_scopes(a.key_scopes)
    key_id, key, key_hash = new_server_key()
    call(a, "admin_add_server_key", json.dumps({
        "game_key": a.server_key, "name": a.key_name, "scopes": scopes, "key_id": key_id, "key_hash": key_hash,
    }))
    print(f"Server key {key_id} ({a.key_name}, {scopes}) for {a.server_key}. It is shown once: store it in your backend's secrets.")
    print(key)


def main(argv=None):
    # Windows consoles may not encode every character of the server's hints.
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    cfg = load(ROOT / "spacetime.json") or {}
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--dir", default="secrets")
    p.add_argument("--server", default=cfg.get("server", "maincloud"))
    p.add_argument("--database", default=cfg.get("database"))
    p.add_argument("--check", action="store_true", help="run the setup check after uploading")
    p.add_argument("--check-only", action="store_true", help="only run the setup check")
    p.add_argument("--check-all", action="store_true",
                   help="check every enabled build of the database and print a summary (exit code 1 if any failed)")
    p.add_argument("--jobs", type=int, default=4, help="--check-all: builds checked at once (default 4)")
    p.add_argument("--offline", action="store_true", help="check the configuration without calling stores")
    p.add_argument("--json", action="store_true", help="print check reports as JSON")
    p.add_argument("--export", metavar="DIR", help="write the database's configuration to DIR (no secret values)")
    p.add_argument("--force", action="store_true", help="--export: overwrite files in a non-empty DIR")
    p.add_argument("--init", action="store_true", help="guided first setup: writes --dir and uploads it")
    p.add_argument("--server-key", metavar="GAME_KEY", help="make a key for the game's backend (/server/*) and print it")
    p.add_argument("--key-name", default="backend", help="--server-key: a name to tell keys apart")
    p.add_argument("--key-scopes", default="read", help="--server-key: comma-separated read, grant, revoke")
    a = p.parse_args(argv)
    if a.init:
        init(a)
        return
    if not a.database or a.database.startswith("<"):
        sys.exit("No database: pass --database or copy spacetime.example.json to spacetime.json and set it")
    if a.export:
        export(a, (ROOT / a.export).resolve(), a.force)
        return
    if a.server_key:
        add_server_key(a)
        return
    if a.check_all:
        if a.jobs < 1:
            p.error("--jobs must be at least 1")
        conn(a)  # resolved once, before the threads
        sys.exit(0 if check_all(a) else 1)
    d = (ROOT / a.dir).resolve()
    if not d.is_dir():
        sys.exit(f"{d} not found (copy seed.example to secrets and fill it in, or run `python seed.py --init`)")

    if not a.check_only:
        seed(a, d)
    if a.check or a.check_only:
        sys.exit(0 if check(a, d) else 1)


if __name__ == "__main__":
    main()
