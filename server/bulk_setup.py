"""Bulk onboarding: creates or updates many builds and their products from two tables, and adds the
products Google Play and the App Store already list. For hundreds of apps; docs/bulk-setup.md.

Usage (from server/, after `spacetime login` as the database owner or an admin with the Configure
role, or with PURSER_TOKEN set):
    python bulk_setup.py --apps apps.csv --products products.csv --dry-run   # print the diff only
    python bulk_setup.py --apps apps.csv --products products.csv
    python bulk_setup.py --apps apps.csv --from-stores     # plus the products the stores list
    python bulk_setup.py --from-stores --dry-run           # every Google Play / App Store build

apps.csv (or a .json list of objects with the same keys), one build per line:
    game_key,platform,store,store_app_id,account,delivery
    my-game,android,,com.example.mygame,my-studio,none
    my-game,ios,,com.example.mygame,my-studio,none
  Required: store_app_id (the build's id in its store: package name, bundle id; any unique id for
  stores without one), platform; a new build also needs delivery (none, spacetimedb, webhook). An
  empty store is the platform's default (web builds name stripe, xsolla or telegram); an empty
  game_key is the store_app_id. The server keys the build `{store}:{store_app_id}`
  (google:com.example.mygame), the app_id of products.csv. Optional columns: spacetime_url,
  spacetime_db, grant_reducer, revoke_reducer, webhook_url, allow_sandbox, enabled, client_consumes,
  apple_refund_mode, client_auth, sns_topic_arn.
products.csv (or .json), one product of one build per line:
    app_id,product_id,kind,catalog_id
    google:com.example.mygame,gems_100,consumable,gems_100
  Required: app_id (the build key), product_id; a new product also needs kind (consumable, non_consumable,
  subscription) unless --from-stores finds it in the store. Optional columns: price (0.99) or
  price_micros, currency, title, description, period (P1W P1M P3M P6M P1Y), apple_group, access.

An empty cell keeps what the server has (a new row gets the default), so a second run with the same
files changes nothing. Every row is checked before anything is written and errors are reported per
line. --from-stores lists the catalog of every Google Play and App Store build of --apps (all such
builds on the server without --apps) with `admin_list_store_products` and adds the products the
server does not have yet, with the store's kind, price and period; a row of products.csv wins over
the store (Google Play does not say which one-time products are non-consumable: list those there).
--dry-run writes nothing and prints what would change (new / changed / unchanged).
"""
import argparse
import csv
import decimal
import io
import json
import pathlib
import re
import sys

import seed
import stdb

ROOT = seed.ROOT

# `GameAppInput` in spacetimedb/src/lib.rs.
APP_FIELDS = ("game_key", "platform", "store", "store_app_id", "account", "delivery", "spacetime_url", "spacetime_db",
              "grant_reducer", "revoke_reducer", "webhook_url", "allow_sandbox", "enabled", "client_consumes",
              "apple_refund_mode", "client_auth", "sns_topic_arn")
APP_BOOLS = {"allow_sandbox", "enabled", "client_consumes"}
# `ProductInput` in spacetimedb/src/lib.rs (+ `price`, a decimal the script turns into micros).
PRODUCT_FIELDS = ("app_id", "product_id", "kind", "catalog_id", "price_micros", "currency", "title", "description",
                  "period", "apple_group", "access")
PRODUCT_COLUMNS = PRODUCT_FIELDS + ("price",)
EMPTY_PRODUCT = {"kind": "", "catalog_id": "", "price_micros": 0, "currency": "", "title": "", "description": "",
                 "period": "", "apple_group": "", "access": ""}
KINDS = ("consumable", "non_consumable", "subscription")  # purser_core::model::kind
DELIVERIES = ("none", "spacetimedb", "spacetimedb_idc", "webhook")  # purser_core::event::delivery
REFUND_MODES = ("off", "inform", "prefer")  # purser_core::consumption::mode
CLIENT_AUTH = ("open", "signed")  # purser_core::client_api::mode
PERIODS = ("P1W", "P1M", "P3M", "P6M", "P1Y")  # purser_core::store_create::PERIODS
# `billing_period_days` of admin_list_store_products (purser_core::catalog) -> period.
PERIOD_OF_DAYS = {7: "P1W", 30: "P1M", 90: "P3M", 180: "P6M", 365: "P1Y"}
DEFAULT_ACCOUNT = "default"  # config::DEFAULT_SCOPE
SELECT_MAX_VALUES = 500  # access::SELECT_MAX_VALUES
# purser_core::catalog limits.
TITLE_MAX, DESCRIPTION_MAX, GROUP_MAX, ACCESS_MAX = 55, 200, 64, 10
SAFE_NAME = re.compile(r"^[A-Za-z0-9._-]*$")  # spacetime_db and reducer names
ID = re.compile(r"^[A-Za-z0-9._-]{1,64}$")  # catalog ids and access levels (purser_core::paywall::is_valid_id)
CURRENCY = re.compile(r"^[A-Z]{3}$")


# ---------------------------------------------------------------------------------------------
# Reading the files
# ---------------------------------------------------------------------------------------------

def to_text(v):
    if v is None:
        return ""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (dict, list)):
        return json.dumps(v)
    return str(v).strip()


def read_rows(path):
    """Rows of a .csv (a header line; blank lines and lines starting with `#` are skipped) or of a
    .json list of objects, each with where it came from (`apps.csv:3`, `apps.json[2]`).
    Returns (rows, errors)."""
    p = pathlib.Path(path)
    if not p.is_file():
        return [], [f"{path}: file not found"]
    try:
        text = p.read_text(encoding="utf-8-sig")  # Excel writes a BOM
    except (OSError, UnicodeDecodeError) as e:
        return [], [f"{p.name}: {e}"]
    return (json_rows if p.suffix.lower() == ".json" else csv_rows)(text, p.name)


def json_rows(text, name):
    try:
        data = json.loads(text)
    except ValueError as e:
        return [], [f"{name}: not valid JSON: {e}"]
    if not isinstance(data, list):
        return [], [f"{name}: expected a list of objects"]
    rows, errors = [], []
    for i, r in enumerate(data):
        if not isinstance(r, dict):
            errors.append(f"{name}[{i}]: expected an object")
            continue
        rows.append((f"{name}[{i}]", {str(k).strip().lower(): to_text(v) for k, v in r.items()}))
    return rows, errors


def csv_rows(text, name):
    rows, errors, header = [], [], None
    reader = csv.reader(io.StringIO(text.lstrip("﻿"), newline=""))
    for cells in reader:
        cells = [c.strip() for c in cells]
        if not any(cells) or cells[0].startswith("#"):
            continue
        where = f"{name}:{reader.line_num}"
        if header is None:
            header = [c.lower() for c in cells]
            if "" in header or len(set(header)) != len(header):
                return [], [f"{where}: the header has an empty or repeated column"]
            continue
        if any(cells[len(header):]):
            errors.append(f"{where}: {len(cells)} cells, the header has {len(header)}")
            continue
        rows.append((where, dict(zip(header, cells + [""] * (len(header) - len(cells))))))
    return rows, errors


def parse_bool(v):
    s = v.strip().lower()
    if s in ("true", "1", "yes", "y"):
        return True
    if s in ("false", "0", "no", "n"):
        return False
    return None


def valid_product_id(s):
    """purser_core::paywall::is_valid_product_id: no whitespace, no `/`, at most 150."""
    return 0 < len(s) <= 150 and not any(c.isspace() or not c.isprintable() or c == "/" for c in s)


# ---------------------------------------------------------------------------------------------
# Checking the rows (no server needed)
# ---------------------------------------------------------------------------------------------

def store_of(platform, store, stores, platforms):
    """(store id, error) as `registry::usable_build_store` decides; `store` empty = the default."""
    p = platforms.get(platform)
    if not p:
        return None, f"platform must be one of {', '.join(platforms)}, not '{platform}'"
    sid = store or p.get("default_store")
    if not sid:
        return None, f"a {platform} build needs a store: {' or '.join(p['stores'])}"
    if sid not in p["stores"]:
        return None, f"store {sid} is not available on {platform} (use {' or '.join(p['stores'])})"
    s = stores.get(sid)
    if not s:
        return None, f"unknown store {sid}"
    if not s.get("implemented"):
        return None, f"{s['name']} is not supported by this server version yet"
    return sid, None


def check_apps(rows, stores, platforms):
    """Rows of apps.csv -> ({build key: (where, row with the non-empty cells)}, errors)."""
    apps, errors, seen = {}, [], {}
    for where, raw in rows:
        errs = []
        unknown = sorted(k for k in raw if k not in APP_FIELDS)
        if unknown:
            errs.append(f"unknown column {', '.join(unknown)} (known: {', '.join(APP_FIELDS)})")
        row = {}
        for k in APP_FIELDS:
            v = raw.get(k, "")
            if v == "":
                continue
            if k in APP_BOOLS:
                b = parse_bool(v)
                if b is None:
                    errs.append(f"{k} must be true or false, not '{v}'")
                else:
                    row[k] = b
            else:
                row[k] = v
        store_app_id, key = row.get("store_app_id", ""), ""
        if not store_app_id:
            errs.append("store_app_id is required")
        elif any(c in store_app_id for c in "/:?&#") or len(store_app_id) > 128 or any(c.isspace() for c in store_app_id):
            errs.append(f"store_app_id '{store_app_id}' must be at most 128 characters without '/', ':', '?', '&', '#' "
                        "or spaces")
        if "platform" not in row:
            errs.append("platform is required")
        else:
            sid, err = store_of(row["platform"], row.get("store", ""), stores, platforms)
            if err:
                errs.append(err)
            else:
                row["store"] = sid
                key = stdb.build_key(row["platform"], store_app_id, sid) if store_app_id else ""
        if key in seen:
            errs.append(f"build {key} is already on {seen[key]}")
        elif key:
            seen[key] = where
        if row.get("delivery", "none") not in DELIVERIES:
            errs.append(f"delivery must be {', '.join(DELIVERIES)}, not '{row['delivery']}'")
        mode = row.get("apple_refund_mode", "")
        if mode and mode not in REFUND_MODES:
            errs.append(f"apple_refund_mode must be {', '.join(REFUND_MODES)}")
        elif mode in ("inform", "prefer") and row.get("store", "apple") != "apple":
            errs.append("apple_refund_mode is for App Store builds")
        if row.get("client_auth", "open") not in CLIENT_AUTH:
            errs.append(f"client_auth must be {' or '.join(CLIENT_AUTH)}")
        for k in ("spacetime_db", "grant_reducer", "revoke_reducer"):
            if not SAFE_NAME.match(row.get(k, "")):
                errs.append(f"{k} may contain only letters, digits, '-', '_', '.'")
        for k in ("spacetime_url", "webhook_url"):
            if k in row and not row[k].startswith("https://"):
                errs.append(f"{k} must start with https://")
        if errs:
            errors.extend(f"{where}: {e}" for e in errs)
            continue
        apps[key] = (where, row)
    return apps, errors


def is_build_key(app_id):
    """`{store}:{store_app_id}` of a known store (`google:com.example.mygame`)."""
    store, _, store_app_id = app_id.partition(":")
    return bool(store_app_id) and any(store in s for s in stdb.PLATFORM_STORES.values())


def check_products(rows):
    """Rows of products.csv -> ({(app_id, product_id): (where, row with the non-empty cells)}, errors)."""
    products, errors, seen = {}, [], {}
    for where, raw in rows:
        errs = []
        unknown = sorted(k for k in raw if k not in PRODUCT_COLUMNS)
        if unknown:
            errs.append(f"unknown column {', '.join(unknown)} (known: {', '.join(PRODUCT_COLUMNS)})")
        row = {k: raw[k] for k in PRODUCT_COLUMNS if raw.get(k, "") != ""}
        app_id, pid = row.get("app_id", ""), row.get("product_id", "")
        if not app_id:
            errs.append("app_id is required")
        elif not is_build_key(app_id):
            errs.append(f"app_id '{app_id}' is not a build key: {{store}}:{{store_app_id}}, e.g. google:com.example.mygame")
        if not pid:
            errs.append("product_id is required")
        elif not valid_product_id(pid):
            errs.append(f"product_id '{pid}' must be at most 150 characters without spaces or '/'")
        elif app_id:
            if (app_id, pid) in seen:
                errs.append(f"{app_id} {pid} is already on {seen[(app_id, pid)]}")
            else:
                seen[(app_id, pid)] = where
        kind = row.get("kind", "")
        if kind and kind not in KINDS:
            errs.append(f"kind must be {', '.join(KINDS)}, not '{kind}'")
        if "price" in row and "price_micros" in row:
            errs.append("set price or price_micros, not both")
        elif "price" in row:
            micros = price_micros(row.pop("price"))
            if micros is None:
                errs.append("price must be a number like 0.99 (at most 6 decimals)")
            else:
                row["price_micros"] = micros
        elif "price_micros" in row:
            try:
                row["price_micros"] = int(row["price_micros"])
                if row["price_micros"] < 0:
                    raise ValueError
            except ValueError:
                errs.append("price_micros must be a whole number, not negative")
        if "currency" in row:
            row["currency"] = row["currency"].upper()
            if not CURRENCY.match(row["currency"]):
                errs.append(f"'{row['currency']}' is not a 3-letter currency code")
        if "period" in row:
            row["period"] = row["period"].upper()
            if row["period"] not in PERIODS:
                errs.append(f"period must be one of {', '.join(PERIODS)}")
        for k in ("period", "apple_group"):
            if k in row and kind and kind != "subscription":
                errs.append(f"{k} is for subscriptions")
        for k, limit in (("title", TITLE_MAX), ("description", DESCRIPTION_MAX), ("apple_group", GROUP_MAX)):
            if len(row.get(k, "")) > limit:
                errs.append(f"{k} is longer than {limit} characters")
        if "catalog_id" in row and not ID.match(row["catalog_id"]):
            errs.append(f"catalog_id '{row['catalog_id']}': use 1-64 of A-Z a-z 0-9 _ - .")
        if "access" in row:
            levels = list(dict.fromkeys(x.strip() for x in row["access"].split(",") if x.strip()))
            bad = [x for x in levels if not ID.match(x)]
            if bad:
                errs.append(f"access level '{bad[0]}': use 1-64 of A-Z a-z 0-9 _ - .")
            elif len(levels) > ACCESS_MAX:
                errs.append(f"at most {ACCESS_MAX} access levels per product")
            row["access"] = ",".join(levels)
        if errs:
            errors.extend(f"{where}: {e}" for e in errs)
            continue
        products[(app_id, pid)] = (where, row)
    return products, errors


def price_micros(text):
    """`0.99` -> 990000; None when it is not a price."""
    try:
        d = decimal.Decimal(text)
        if not d.is_finite() or d < 0:
            return None
        m = d * 1_000_000
        return int(m) if m == m.to_integral_value() else None
    except (decimal.InvalidOperation, ValueError):
        return None


# ---------------------------------------------------------------------------------------------
# What to change (the server's rows are given)
# ---------------------------------------------------------------------------------------------

def plan_apps(apps, server_apps):
    """Each build of the file merged over the server's row (both by build key). Returns (plan, errors);
    a plan entry is {status: new | changed | same, app_id (the key), where, row (what
    admin_upsert_game_app gets), changes}."""
    plan, errors = [], []
    for app_id, (where, row) in apps.items():
        old = server_apps.get(app_id)
        want = {k: old[k] for k in APP_FIELDS if k in old} if old else {}
        want.update(row)
        if not want.get("account"):
            want["account"] = DEFAULT_ACCOUNT
        if not want.get("game_key"):
            want["game_key"] = want["store_app_id"]
        if want.get("spacetime_url"):
            want["spacetime_url"] = want["spacetime_url"].rstrip("/")
        delivery = want.get("delivery", "")
        if not delivery:
            errors.append(f"{where}: a new build needs delivery ({', '.join(DELIVERIES)})")
            continue
        if delivery == "spacetimedb" and not all(want.get(k) for k in ("spacetime_url", "spacetime_db", "grant_reducer")):
            errors.append(f"{where}: spacetimedb delivery needs spacetime_url, spacetime_db, grant_reducer")
            continue
        if delivery == "webhook" and not str(want.get("webhook_url", "")).startswith("https://"):
            errors.append(f"{where}: webhook delivery needs an https webhook_url")
            continue
        changes = {} if not old else {k: (old.get(k), v) for k, v in want.items() if old.get(k) != v}
        status = "new" if not old else "changed" if changes else "same"
        plan.append({"status": status, "app_id": app_id, "where": where, "row": want, "changes": changes})
    return plan, errors


def product_row(app_id, product_id, *layers):
    """A full admin_upsert_products row: the layers over the empty product, normalized the way the
    reducer stores it (so that an unchanged product compares equal)."""
    p = dict(EMPTY_PRODUCT, app_id=app_id, product_id=product_id)
    for layer in layers:
        p.update({k: v for k, v in (layer or {}).items() if k in PRODUCT_FIELDS and k not in ("app_id", "product_id")})
    p["price_micros"] = int(p["price_micros"] or 0)
    p["currency"] = (p["currency"] or "").upper()
    if p["kind"] == "subscription":
        p["period"] = (p["period"] or "").upper()
    else:
        p["period"] = p["apple_group"] = ""
    if p["catalog_id"] == product_id:
        p["catalog_id"] = ""
    return p


def store_new_products(listing):
    """Products `admin_list_store_products` lists as new (not on the server) -> {product_id: fields}."""
    out = {}
    for r in listing.get("products") or []:
        pid = r.get("product_id") or ""
        if r.get("status") != "new" or not valid_product_id(pid):
            continue
        kind = r.get("kind") if r.get("kind") in KINDS else "consumable"
        out[pid] = {"kind": kind, "price_micros": int(r.get("price_micros") or 0), "currency": r.get("currency") or "",
                    "period": PERIOD_OF_DAYS.get(int(r.get("billing_period_days") or 0), "") if kind == "subscription" else ""}
    return out


def plan_products(products, known_apps, server_products, store_new=None, kind_later=()):
    """Products of the file merged over the server's rows (over the store's for products the server
    does not have), plus the store's new products the file does not name.
    `server_products`: {app_id: {product_id: row}}; `store_new`: {app_id: {product_id: fields}};
    `kind_later`: builds whose store catalog is listed later (a new product there may leave out its
    kind, it is left out of this plan). Returns (plan, errors, warnings); a plan entry is
    {status, app_id, product_id, where, row, changes}; rows with errors are not in the plan."""
    store_new = store_new or {}
    plan, errors, warnings = [], [], []
    for (app_id, pid), (where, row) in products.items():
        if app_id not in known_apps:
            errors.append(f"{where}: no build {app_id} in the apps file or on the server")
            continue
        old = server_products.get(app_id, {}).get(pid)
        from_store = None if old else store_new.get(app_id, {}).get(pid)
        want = product_row(app_id, pid, old or from_store, row)
        if not want["kind"]:
            if app_id not in kind_later:
                errors.append(f"{where}: a new product needs kind ({', '.join(KINDS)})"
                              + (", or the store does not list it" if app_id in store_new else ""))
            continue
        if want["access"] and want["kind"] == "consumable":
            errors.append(f"{where}: a consumable unlocks no access level")
            continue
        if want["price_micros"] > 0 and not want["currency"]:
            errors.append(f"{where}: a price needs a currency")
            continue
        plan.append(entry(want, where, old))

    # One catalog id per product of a build (admin_upsert_products refuses a second one).
    cids = {}
    for app_id, rows in server_products.items():
        for pid, r in rows.items():
            cids.setdefault(app_id, {})[pid] = r.get("catalog_id") or pid
    for e in plan:
        cids.setdefault(e["app_id"], {})[e["product_id"]] = e["row"]["catalog_id"] or e["product_id"]
    clashing = []
    for e in plan:
        cid = e["row"]["catalog_id"] or e["product_id"]
        others = sorted(p for p, c in cids[e["app_id"]].items() if c == cid and p != e["product_id"])
        if others:
            errors.append(f"{e['where']}: {e['app_id']} has the catalog id {cid} on {others[0]} too")
            clashing.append(e)
    plan = [e for e in plan if e not in clashing]

    named = set(products)
    for app_id in sorted(store_new):
        for pid, fields in sorted(store_new[app_id].items()):
            if (app_id, pid) in named or pid in server_products.get(app_id, {}):
                continue
            taken = [p for p, c in cids.get(app_id, {}).items() if c == pid and p != pid]
            if taken:
                warnings.append(f"{app_id} {pid}: not added from the store, the catalog id is taken by {taken[0]}")
                continue
            plan.append(entry(product_row(app_id, pid, fields), "store", None))
    return plan, errors, warnings


def entry(row, where, old):
    if not old:
        return {"status": "new", "app_id": row["app_id"], "product_id": row["product_id"], "where": where,
                "row": row, "changes": {}}
    before = product_row(row["app_id"], row["product_id"], old)
    changes = {k: (before[k], v) for k, v in row.items() if before[k] != v}
    return {"status": "changed" if changes else "same", "app_id": row["app_id"], "product_id": row["product_id"],
            "where": where, "row": row, "changes": changes}


# ---------------------------------------------------------------------------------------------
# Printing
# ---------------------------------------------------------------------------------------------

def show(v):
    return repr(v) if isinstance(v, str) else json.dumps(v)


def describe_changes(changes):
    return ", ".join(f"{k} {show(a)} -> {show(b)}" for k, (a, b) in changes.items())


def counts(plan, extra=""):
    n = {s: sum(1 for e in plan if e["status"] == s) for s in ("new", "changed", "same")}
    return f"{n['new']} new{extra}, {n['changed']} changed, {n['same']} unchanged"


def print_apps(plan, verbose):
    print(f"Builds: {counts(plan)}")
    for e in plan:
        r = e["row"]
        if e["status"] == "new":
            print(f"  new      {e['app_id']} ({r['platform']}, app {r['game_key']}, "
                  f"account {r['account']}, delivery {r['delivery']})")
        elif e["status"] == "changed":
            print(f"  changed  {e['app_id']}: {describe_changes(e['changes'])}")
        elif verbose:
            print(f"  same     {e['app_id']}")


def price_text(r):
    return f"{r['price_micros'] / 1e6:g} {r['currency']}" if r["price_micros"] else "no price"


def print_products(plan, verbose):
    from_store = sum(1 for e in plan if e["where"] == "store")
    print(f"Products: {counts(plan, f' ({from_store} from the stores)' if from_store else '')}")
    for e in plan:
        r, name = e["row"], f"{e['app_id']} {e['product_id']}"
        if e["status"] == "new":
            extra = [r["kind"], price_text(r)]
            if r["period"]:
                extra.append(r["period"])
            if r["catalog_id"]:
                extra.append(f"catalog id {r['catalog_id']}")
            if e["where"] == "store":
                extra.append("from the store")
            print(f"  new      {name} ({', '.join(extra)})")
        elif e["status"] == "changed":
            print(f"  changed  {name}: {describe_changes(e['changes'])}")
        elif verbose:
            print(f"  same     {name}")


# ---------------------------------------------------------------------------------------------
# Server calls
# ---------------------------------------------------------------------------------------------

def select(a, table, column="", values=()):
    """Rows through `admin_select` (every admin, not only the owner), at most 500 values per call."""
    base, tok = seed.conn(a)
    values = sorted(set(values))
    if column:
        queries = [{"table": table, "where": {"column": column, "equals": values[i:i + SELECT_MAX_VALUES]}}
                   for i in range(0, len(values), SELECT_MAX_VALUES)]
    else:
        queries = [{"table": table}]
    rows = []
    for q in queries:
        try:
            v = stdb.call_raw(base, a.database, tok, "admin_select", json.dumps(q))
        except stdb.StdbError as e:
            sys.exit(f"admin_select {table} failed:\n{(e.body or str(e)).strip()}")
        if isinstance(v, dict) and "error" in v:
            sys.exit(f"admin_select {table} failed: {v['error']}")
        rows.extend(v or [])
    return rows


def list_store(a, app_id):
    """admin_list_store_products -> (listing, error)."""
    base, tok = seed.conn(a)
    try:
        v = stdb.call_raw(base, a.database, tok, "admin_list_store_products", app_id)
    except stdb.StdbError as e:
        return None, (e.body or str(e)).strip()
    if not isinstance(v, dict):
        return None, f"unexpected answer {str(v)[:200]}"
    if "error" in v:
        return None, str(v["error"])
    return v, None


def upload_products(a, rows, batch):
    """admin_upsert_products in batches. A refused batch is rolled back as a whole: its rows are sent
    one by one to find the ones the server refuses. Returns the failures."""
    failed = []
    for i in range(0, len(rows), batch):
        chunk = rows[i:i + batch]
        if seed.try_call(a, "admin_upsert_products", json.dumps(chunk)) is None:
            continue
        for r in chunk:
            err = seed.try_call(a, "admin_upsert_products", json.dumps([r]))
            if err is not None:
                failed.append(f"product {r['app_id']} {r['product_id']}: {err}")
    return failed


# ---------------------------------------------------------------------------------------------

def main(argv=None):
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    cfg = seed.load(ROOT / "spacetime.json") or {}
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--server", default=cfg.get("server", "maincloud"))
    p.add_argument("--database", default=cfg.get("database"))
    p.add_argument("--apps", metavar="FILE", help="builds: .csv or .json")
    p.add_argument("--products", metavar="FILE", help="products: .csv or .json")
    p.add_argument("--from-stores", action="store_true",
                   help="add the products Google Play / the App Store list and the server does not have")
    p.add_argument("--dry-run", action="store_true", help="print what would change, write nothing")
    p.add_argument("--verbose", "-v", action="store_true", help="list unchanged rows too")
    p.add_argument("--jobs", type=int, default=4, help="store catalogs listed at once (default 4)")
    p.add_argument("--batch", type=int, default=100, help="products per admin_upsert_products call (default 100)")
    a = p.parse_args(argv)
    if not (a.apps or a.products or a.from_stores):
        p.error("nothing to do: pass --apps, --products and/or --from-stores")
    if a.batch < 1 or a.jobs < 1:
        p.error("--batch and --jobs must be at least 1")

    stores, platforms = seed.registry()
    errors = []
    app_rows, e = read_rows(a.apps) if a.apps else ([], [])
    errors += e
    apps, e = check_apps(app_rows, stores, platforms)
    errors += e
    product_rows, e = read_rows(a.products) if a.products else ([], [])
    errors += e
    products, e = check_products(product_rows)
    errors += e
    if errors:
        return fail(errors)

    if not a.database or a.database.startswith("<"):
        sys.exit("No database: pass --database or copy spacetime.example.json to spacetime.json and set it")
    seed.conn(a)  # resolved once, before any thread
    server_apps = {r["app_id"]: r for r in select(a, "game_app")}
    app_plan, errors = plan_apps(apps, server_apps)
    store_of_build = {i: r.get("store", "") for i, r in server_apps.items()}
    store_of_build.update({i: row["store"] for i, (_, row) in apps.items()})
    importable = {sid for sid, s in stores.items() if s.get("product_import")}
    scope = sorted(i for i in (apps or server_apps) if store_of_build.get(i) in importable) if a.from_stores else []
    known = set(server_apps) | set(apps)
    on_server = sorted(i for i in {k[0] for k in products} | set(scope) if i in server_apps)
    server_products = {}
    for r in select(a, "product", "app_id", on_server):
        server_products.setdefault(r["app_id"], {})[r["product_id"]] = r
    _, e, _ = plan_products(products, known, server_products, kind_later=set(scope))
    errors += e
    if errors:
        return fail(errors)

    print(f"{a.server}/{a.database}{' (dry run: nothing is written)' if a.dry_run else ''}")
    failures, broken = [], set()
    if a.apps:
        print_apps(app_plan, a.verbose)
        if not a.dry_run:
            for e in app_plan:
                if e["status"] == "same":
                    continue
                err = seed.try_call(a, "admin_upsert_game_app", json.dumps(e["row"]))
                if err is not None:
                    failures.append(f"build {e['app_id']} ({e['where']}): {err}")
                    broken.add(e["app_id"])

    store_new = {}
    if a.from_stores:
        live = [i for i in scope if i not in broken and (i in server_apps or not a.dry_run)]
        later = [i for i in scope if i not in server_apps and a.dry_run]
        print(f"Store catalogs: {len(live)} build(s)" + (f", {len(later)} new build(s) are listed after they are created"
                                                         if later else ""))
        for app_id, (listing, err) in zip(live, seed.pmap(lambda i: list_store(a, i), live, a.jobs)):
            if err:
                failures.append(f"store catalog of {app_id}: {err}")
            else:
                store_new[app_id] = store_new_products(listing)

    # Now with the stores' products; a product whose kind only the store could give and did not, or
    # whose catalog was not listed, is a failure (the rest goes on).
    unlisted = set(scope) - set(store_new)
    product_plan, e, warnings = plan_products(products, known, server_products, store_new, kind_later=unlisted)
    failures += e
    waiting = [where for (app_id, pid), (where, row) in products.items()
               if app_id in unlisted and app_id not in broken
               and not product_row(app_id, pid, server_products.get(app_id, {}).get(pid), row)["kind"]]
    product_plan = [e for e in product_plan if e["app_id"] not in broken]
    if a.products or a.from_stores:
        print_products(product_plan, a.verbose)
        for w in warnings:
            print(f"  skip     {w}")
        if waiting and a.dry_run:
            print(f"  later    {len(waiting)} product(s) without a kind take it from the store catalog of a new build "
                  f"({', '.join(waiting[:3])}{', ...' if len(waiting) > 3 else ''})")
        elif waiting:
            failures += [f"{w}: no kind, and the build's store catalog was not listed" for w in waiting]
        if not a.dry_run:
            failures += upload_products(a, [e["row"] for e in product_plan if e["status"] != "same"], a.batch)

    if failures:
        print(f"\n{len(failures)} failure(s):")
        for f in failures:
            print(f"  {f}")
        print("Fix them and run the same command again: what is done already stays unchanged.")
    if a.dry_run:
        print("\nDry run: nothing was written.")
    return 1 if failures else 0


def fail(errors):
    print(f"{len(errors)} error(s), nothing was written:", file=sys.stderr)
    for e in errors:
        print(f"  {e}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
