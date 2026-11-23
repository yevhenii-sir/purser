"""Backs up a purser database to JSON files and restores them into another (or the same) one.

Use it around a release whose notes say "publish with --delete-data", and for regular backups:

    python backup.py backup  --server maincloud --database <your-billing-db> --dir backups/2026-11-15
    spacetime publish --delete-data ...            # the new version
    python backup.py restore --server maincloud --database <your-billing-db> --dir backups/2026-11-15

Everything goes through the owner's `/sql` (SELECT to back up; DELETE and INSERT to restore), so the
token must be the database owner's: `PURSER_TOKEN`, else the `spacetime login` one.

What is copied: every table except caches (access tokens, rate limit buckets), queues of work in
progress (integration, alert and Apple refund outboxes) and the worker's schedules, which the new
module creates itself. Restoring replaces the rows of each copied table, so restore into a freshly
published database; the backup's format version must match the server's (no conversion of older
formats yet). Auto-increment ids of logs are given anew (their order is kept); alert channels keep
their secrets (`alert-<id>` scopes are rewritten to the new ids).

Exit code 1 on any error. `--dry-run` (restore) prints what would be written.
"""
import argparse
import json
import os
import sys

from stdb import StdbError, call_raw, request, server_url, sql_str, token

FORMAT_VERSION = 1

# Not backed up: caches, queued work and the schedules the module creates on publish.
SKIP = {
    "access_token_cache",
    "rate_bucket",
    "integration_outbox",
    "alert_outbox",
    "apple_consumption_outbox",
    "retry_schedule",
    "revenue_rebuild_tick",
    "store_create_tick",
    # Derived from the ledger: rebuilt after a restore (`admin_reindex_ledger`).
    "pending_purchase",
    "owned_purchase",
}
# Auto-increment primary keys that nothing refers to: inserted as 0, so the new database's sequence
# gives them ids (inserting the old ids would collide with ids it gives later).
FRESH_IDS = {"purchase_log": "id", "admin_audit": "id", "alert_log": "id", "store_create_job": "id"}
# Rows per INSERT statement.
BATCH = 200


def sql(base, db, tok, query):
    return json.loads(request("POST", f"{base}/v1/database/{db}/sql", tok, query, timeout=300))


def tables(base, db, tok):
    """Names of the database's user tables (the owner reads `st_table`)."""
    out = sql(base, db, tok, "SELECT table_name, table_type FROM st_table")[0]
    names = []
    for row in out["rows"]:
        name, kind = row[0], row[1]
        kind = kind if isinstance(kind, str) else json.dumps(kind)
        if "user" in kind.lower() or "User" in kind:
            names.append(name)
    return sorted(n for n in names if not n.startswith("st_"))


def column_names(schema):
    return [e["name"].get("some", e["name"]) if isinstance(e["name"], dict) else e["name"] for e in schema["elements"]]


def literal(value, algebraic_type):
    """A SQL literal for a value of `/sql` JSON."""
    (kind, inner), = algebraic_type.items()
    if kind == "String":
        return sql_str(value)
    if kind == "Bool":
        return "true" if value else "false"
    if kind in ("I8", "U8", "I16", "U16", "I32", "U32", "I64", "U64", "I128", "U128", "I256", "U256"):
        return str(int(value))
    if kind in ("F32", "F64"):
        return repr(float(value))
    if kind == "Product":
        elements = inner["elements"]
        names = [e["name"].get("some") if isinstance(e["name"], dict) else e["name"] for e in elements]
        # Identity / ConnectionId / Timestamp: one special field.
        if len(elements) == 1 and names[0] and names[0].startswith("__"):
            v = value[0] if isinstance(value, list) else value
            if isinstance(v, str):
                return v if v.startswith("0x") else sql_str(v)
            if names[0] in ("__identity__", "__connection_id__"):
                return "0x" + format(int(v), "064x" if names[0] == "__identity__" else "032x")
            return str(int(v))
    raise ValueError(f"cannot restore a column of type {json.dumps(algebraic_type)}")


def backup(a, base, tok):
    os.makedirs(a.dir, exist_ok=True)
    if os.listdir(a.dir) and not a.force:
        sys.exit(f"{a.dir} is not empty (use --force to overwrite)")
    try:
        health = json.loads(request("GET", f"{base}/v1/database/{a.database}/route/healthz", tok) or "{}")
    except StdbError as e:
        # A stopped worker answers 503 with the same body.
        health = json.loads(e.body) if getattr(e, "body", "") else {}
    manifest = {"format_version": FORMAT_VERSION, "module_version": health.get("version", ""), "tables": {}}
    for name in tables(base, a.database, tok):
        if name in SKIP:
            continue
        result = sql(base, a.database, tok, f"SELECT * FROM {name}")[0]
        with open(os.path.join(a.dir, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump({"schema": result["schema"], "rows": result["rows"]}, f)
        manifest["tables"][name] = len(result["rows"])
        print(f"backup   {name}: {len(result['rows'])} rows")
    with open(os.path.join(a.dir, "manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2)
    print(f"done     {sum(manifest['tables'].values())} rows of {len(manifest['tables'])} tables in {a.dir} "
          f"(module {manifest['module_version'] or 'unknown'})")


def insert_statements(name, data, rename_scope=None):
    schema, rows = data["schema"], data["rows"]
    cols = column_names(schema)
    types = [e["algebraic_type"] for e in schema["elements"]]
    fresh = FRESH_IDS.get(name)
    if fresh:
        rows = sorted(rows, key=lambda r: r[cols.index(fresh)])
    values = []
    for row in rows:
        row = list(row)
        if fresh:
            row[cols.index(fresh)] = 0
        if rename_scope:
            row = rename_scope(cols, row)
        values.append("(" + ", ".join(literal(v, t) for v, t in zip(row, types)) + ")")
    for i in range(0, len(values), BATCH):
        yield f"INSERT INTO {name} ({', '.join(cols)}) VALUES " + ", ".join(values[i:i + BATCH])


def reindex(base, db, tok):
    """Rebuilds the tables derived from the ledger rows (the worker's queue, the players' owned
    purchases). The request is a `server_state` row, so the worker does it even when the caller is
    not an admin of the restored database; an owner admin gets it done right away."""
    sql(base, db, tok, "DELETE FROM server_state WHERE key = 'ledger_reindex'")
    sql(base, db, tok, "INSERT INTO server_state (key, value, updated_at_micros) VALUES ('ledger_reindex', 'clear', 0)")
    for _ in range(100000):
        try:
            out = call_raw(base, db, tok, "admin_reindex_ledger") or {}
        except StdbError as e:
            out = {"error": str(e)}
        if out.get("error"):
            print("reindex  queued: the worker rebuilds the queue and the owned purchases within minutes "
                  f"({out['error']})")
            return
        if out.get("done"):
            print("reindex  worker queue and owned purchases rebuilt from the ledger")
            return
    sys.exit("admin_reindex_ledger did not finish: the worker continues it")


def restore(a, base, tok):
    with open(os.path.join(a.dir, "manifest.json"), encoding="utf-8") as f:
        manifest = json.load(f)
    if manifest.get("format_version") != FORMAT_VERSION:
        sys.exit(f"backup format {manifest.get('format_version')} is not {FORMAT_VERSION}: restore it with the purser "
                 "version that made it")
    present = set(tables(base, a.database, tok))
    missing = [t for t in manifest["tables"] if t not in present]
    if missing:
        sys.exit(f"the database has no table {', '.join(missing)}: is it the same purser version as the backup?")
    run = (lambda q: print(q[:200] + ("..." if len(q) > 200 else ""))) if a.dry_run else (lambda q: sql(base, a.database, tok, q))

    # Alert channels first: they get new ids, and their secrets' scopes follow.
    order = sorted(manifest["tables"], key=lambda t: (t != "alert_channel", t == "credential", t))
    channel_ids = {}
    for name in order:
        with open(os.path.join(a.dir, f"{name}.json"), encoding="utf-8") as f:
            data = json.load(f)
        run(f"DELETE FROM {name}")
        if name == "alert_channel":
            cols = column_names(data["schema"])
            for row in sorted(data["rows"], key=lambda r: r[cols.index("id")]):
                old = row[cols.index("id")]
                fresh = list(row)
                fresh[cols.index("id")] = 0
                stmt, = insert_statements(name, {"schema": data["schema"], "rows": [fresh]})
                run(stmt)
                if not a.dry_run:
                    got = sql(base, a.database, tok, f"SELECT id FROM alert_channel WHERE name = {sql_str(row[cols.index('name')])}")[0]["rows"]
                    channel_ids[f"alert-{old}"] = f"alert-{got[0][0]}"
            print(f"restore  {name}: {len(data['rows'])} rows")
            continue

        def rename_scope(cols, row):
            if name != "credential" or not channel_ids:
                return row
            key = row[cols.index("key")]
            scope, sep, rest = key.partition("/")
            if scope in channel_ids:
                row[cols.index("key")] = channel_ids[scope] + sep + rest
            return row

        for stmt in insert_statements(name, data, rename_scope):
            run(stmt)
        print(f"restore  {name}: {len(data['rows'])} rows")
    if not a.dry_run:
        reindex(base, a.database, tok)
    print("done     the worker continues unfinished purchases within a minute (admin page: Overview)")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("action", choices=["backup", "restore"])
    p.add_argument("--server", default="maincloud", help="CLI server nickname or URL")
    p.add_argument("--database", required=True)
    p.add_argument("--dir", required=True)
    p.add_argument("--force", action="store_true", help="backup: write into a non-empty --dir")
    p.add_argument("--dry-run", action="store_true", help="restore: print the statements instead of running them")
    a = p.parse_args()
    base = a.server if a.server.startswith("http") else server_url(a.server)
    try:
        (backup if a.action == "backup" else restore)(a, base, token())
    except StdbError as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
