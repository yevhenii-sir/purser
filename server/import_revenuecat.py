"""Imports purchase history from a RevenueCat transactions export into the ledger.

RevenueCat's Scheduled Data Exports deliver the transactions feed as gzip-compressed CSV
(`transactions_<timestamp>.csv.gz`, one row per transaction: every renewal is its own row,
`renewal_number` counts them). This script maps the rows to ledger rows and sends them to
`admin_import_purchases` (owner only, 500 per call; rows already in the ledger are skipped), so
`/entitlements` knows the players' subscriptions and non-consumables before their clients restore.

    python import_revenuecat.py --database <your-billing-db> --csv transactions.csv.gz \\
        --app apple=com.example.mygame --app google=com.example.mygame --dry-run

Mapping: `store` app_store → apple, play_store → google, stripe → stripe, promotional → manual.
`--app <store>=<id>` names the build each store's rows belong to: for apple, google and stripe the
build's id in that store (bundle id, package name, the Stripe build's id), so `apple=com.example.mygame`
is the build apple:com.example.mygame; promotional grants (`manual`) go to a build of any store,
named by its key (`manual=google:com.example.mygame`). Rows of stores without one are counted and
skipped. Products must exist in the build (`--skip-unknown` skips rows of other
products). Imported rows are final: nothing is delivered to the game again and no integration
event is sent. Google rows have no purchase token in the export, so store notifications find
them only after the player's client verifies or restores the purchase once.
"""
import argparse
import csv
import datetime
import gzip
import io
import json
import sys

from stdb import StdbError, build_key, call_raw, select, server_url, sql_str, token

STORES = {"app_store": "apple", "play_store": "google", "stripe": "stripe", "promotional": "manual"}
REQUIRED = [
    "store", "store_transaction_id", "original_store_transaction_id", "product_identifier",
    "rc_original_app_user_id", "start_time", "end_time", "is_auto_renewable", "is_trial_period",
    "is_sandbox", "refunded_at", "purchased_currency", "renewal_number",
]
# Read when present (RevenueCat lets you choose the exported columns).
OPTIONAL = [
    "rc_last_seen_app_user_id_alias", "purchase_price_in_purchased_currency", "price_in_purchased_currency",
    "country", "ownership_type", "updated_at", "effective_end_time",
]
BATCH = 500
# RevenueCat's store of a row -> the platform of its Purser build (`manual`: any build, named by key).
PLATFORM_OF = {"apple": "ios", "google": "android", "stripe": "web"}


def open_csv(path):
    with open(path, "rb") as f:
        raw = f.read()
    if raw[:2] == b"\x1f\x8b":
        raw = gzip.decompress(raw)
    return csv.DictReader(io.StringIO(raw.decode("utf-8-sig")))


def micros(text):
    """`2023-01-01 08:27:06` (UTC, optionally with fractions or an ISO `T`/`Z`) → Unix micros; 0 when empty."""
    text = (text or "").strip()
    if not text:
        return 0
    text = text.replace("T", " ").replace("Z", "").split("+")[0]
    for fmt in ("%Y-%m-%d %H:%M:%S.%f", "%Y-%m-%d %H:%M:%S"):
        try:
            d = datetime.datetime.strptime(text, fmt).replace(tzinfo=datetime.timezone.utc)
            return int(d.timestamp() * 1_000_000)
        except ValueError:
            pass
    raise ValueError(f"unreadable time {text!r}")


def flag(text):
    return (text or "").strip().lower() in ("true", "t", "1", "yes")


def amount_micros(text):
    text = (text or "").strip()
    return int(round(float(text) * 1_000_000)) if text else 0


def latest_versions(rows):
    """An export may hold several versions of a transaction (refunds, billing issues update it):
    the one with the latest `updated_at` per (store_transaction_id, renewal_number)."""
    best = {}
    for r in rows:
        key = (r["store"], r["store_transaction_id"], r["renewal_number"])
        if key not in best or (r.get("updated_at") or "") >= (best[key].get("updated_at") or ""):
            best[key] = r
    return list(best.values())


def map_row(r, app_id, kinds, now_micros, user_column="rc_original_app_user_id"):
    """One export row as a ledger row (`admin_import_purchases` input); None for a product the
    build does not have."""
    store = STORES[r["store"]]
    product_id = r["product_identifier"].strip()
    if product_id not in kinds:
        return None
    renewal = int(r.get("renewal_number") or 1)
    tid = r["store_transaction_id"].strip()
    # Stripe repeats the transaction id for every renewal of a subscription.
    if store == "stripe" and renewal > 1:
        tid = f"{tid}.{renewal}"
    original = (r.get("original_store_transaction_id") or "").strip() or tid
    created = micros(r["start_time"])
    end = micros(r.get("effective_end_time") or r.get("end_time"))
    renewable = flag(r["is_auto_renewable"])
    kind = "subscription" if renewable else kinds[product_id]
    trial = flag(r["is_trial_period"])
    gross = amount_micros(r.get("purchase_price_in_purchased_currency") or r.get("price_in_purchased_currency"))
    net = amount_micros(r.get("price_in_purchased_currency")) if (r.get("price_in_purchased_currency") or "").strip() else gross
    refunded = bool((r.get("refunded_at") or "").strip())
    if refunded:
        status = "revoked"
    elif kind == "subscription" and end and end <= now_micros:
        status = "expired"
    else:
        status = "completed"
    family = (r.get("ownership_type") or "").strip().upper() == "FAMILY_SHARED"
    price = 0 if trial or family else gross
    return {
        "id": f"{store}:{tid}",
        "store": store,
        "app_id": app_id,
        "product_id": product_id,
        "kind": kind,
        "transaction_id": tid,
        "original_transaction_id": original,
        "purchase_token": "",
        "user_id": (r.get(user_column) or r["rc_original_app_user_id"]).strip(),
        "status": status,
        "reason": "renewal" if renewal > 1 or (original != tid and store != "stripe") else "purchase",
        "store_action": "",
        "expires_at_ms": end // 1000 if kind == "subscription" else 0,
        "environment": "sandbox" if flag(r["is_sandbox"]) else "production",
        "price_micros": price,
        "currency": (r.get("purchased_currency") or "").strip().upper(),
        "was_granted": True,
        "attempts": 0,
        "next_attempt_at_micros": 0,
        "last_error": "",
        "created_at_micros": created,
        "updated_at_micros": micros(r.get("updated_at")) or created,
        "country": (r.get("country") or "").strip().upper()[:2],
        "trial": trial,
        "family_shared": family,
        # Partial refunds: the export's net price is below the gross one.
        "refunded_micros": max(0, price - net) if price and not refunded else 0,
    }


def parse_apps(items):
    """`--app` values -> {store: build key}; exits on a bad one."""
    apps = {}
    for item in items:
        store, _, value = item.partition("=")
        if store not in STORES.values() or not value:
            sys.exit(f"--app {item}: use <{'|'.join(STORES.values())}>=<id>")
        if store == "manual":
            # Promotional grants belong to no store: the build is named by its key.
            if ":" not in value:
                sys.exit(f"--app {item}: name the build of promotional grants by its key, e.g. manual=google:com.example.mygame")
            apps[store] = value
            continue
        try:
            apps[store] = build_key(PLATFORM_OF[store], value, store)
        except ValueError as e:
            sys.exit(f"--app {item}: {e}")
    return apps


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--server", default="maincloud", help="CLI server nickname or URL")
    p.add_argument("--database", required=True)
    p.add_argument("--csv", required=True, help="RevenueCat transactions export (.csv or .csv.gz)")
    p.add_argument("--app", action="append", default=[], metavar="STORE=ID",
                   help="build of a store's rows: apple=<bundle id>, google=<package>, stripe=<id>, "
                        "manual=<build key> (repeat)")
    p.add_argument("--user-column", default="rc_original_app_user_id",
                   help="column with the player id the game uses (or rc_last_seen_app_user_id_alias)")
    p.add_argument("--skip-unknown", action="store_true", help="skip rows of products the build does not have")
    p.add_argument("--dry-run", action="store_true", help="map and count, import nothing")
    a = p.parse_args()

    apps = parse_apps(a.app)
    if not apps:
        sys.exit("name at least one build: --app apple=com.example.mygame")

    reader = open_csv(a.csv)
    missing = [c for c in REQUIRED if c not in (reader.fieldnames or [])]
    if missing:
        sys.exit(f"the export lacks columns {', '.join(missing)}: select them in RevenueCat's export settings")
    rows = latest_versions(list(reader))

    base = a.server if a.server.startswith("http") else server_url(a.server)
    tok = token()
    kinds_of = {}
    for store, app_id in apps.items():
        found = select(base, a.database, tok, f"SELECT product_id, kind FROM product WHERE app_id = {sql_str(app_id)}")
        if not found:
            sys.exit(f"build {app_id} has no products here: create the build and its products first")
        kinds_of[store] = {r["product_id"]: r["kind"] for r in found}

    now = int(datetime.datetime.now(datetime.timezone.utc).timestamp() * 1_000_000)
    out, other_store, unknown = [], {}, {}
    for r in rows:
        store = STORES.get(r["store"])
        if store not in apps:
            other_store[r["store"]] = other_store.get(r["store"], 0) + 1
            continue
        row = map_row(r, apps[store], kinds_of[store], now, a.user_column)
        if row is None:
            unknown[r["product_identifier"]] = unknown.get(r["product_identifier"], 0) + 1
            continue
        out.append(row)
    for s, n in sorted(other_store.items()):
        print(f"skipped  {n} rows of store {s} (no --app for it)")
    if unknown:
        listed = ", ".join(f"{p} ({n})" for p, n in sorted(unknown.items()))
        if not a.skip_unknown:
            sys.exit(f"products not in the build: {listed}. Add them, or pass --skip-unknown")
        print(f"skipped  rows of products not in the build: {listed}")
    by_status = {}
    for r in out:
        by_status[r["status"]] = by_status.get(r["status"], 0) + 1
    print(f"mapped   {len(out)} rows: " + ", ".join(f"{s} {n}" for s, n in sorted(by_status.items())))
    if a.dry_run:
        print(json.dumps(out[:3], indent=2))
        return
    for i in range(0, len(out), BATCH):
        try:
            call_raw(base, a.database, tok, "admin_import_purchases", json.dumps(out[i:i + BATCH]))
        except StdbError as e:
            sys.exit(f"batch {i // BATCH + 1}: {e}")
        print(f"imported {min(i + BATCH, len(out))} / {len(out)}")
    print("done     the revenue aggregates are rebuilt by the worker once the import has stopped")


if __name__ == "__main__":
    main()
