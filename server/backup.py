"""Backs up a purser database to JSON files and restores them into another (or the same) one.

Use it around a release whose notes say "publish with --delete-data", and for regular backups:

    python backup.py backup  --server maincloud --database <your-billing-db> --dir backups/2026-11-15
    spacetime publish --delete-data ...            # the new version
    python backup.py restore --server maincloud --database <your-billing-db> --dir backups/2026-11-15

Everything goes through the owner's `/sql` (SELECT to back up; DELETE and INSERT to restore), so the
token must be the database owner's: `PURSER_TOKEN`, else the `spacetime login` one.

What is copied: every table except caches (access tokens, rate limit buckets), queues of work in
progress (integration, alert and Apple refund outboxes), the worker's schedules, which the new
module creates itself, inter-database calls in flight, and tables derived from the ledger (rebuilt
after a restore). Large tables are read in pages the module reads through an index
(`admin_backup_page`), so no answer holds a whole ledger and no read scans it; a module without that
procedure is read in windows of an indexed column with `/sql`.

Restoring replaces the rows of each copied table, so restore into a freshly published database. A
newer module may have added columns: they get their zero value (empty text, 0, false); columns the
new module removed are dropped. Auto-increment ids of logs are given anew (their order is kept);
alert channels keep their secrets (`alert-<id>` scopes are rewritten to the new ids). A restore that
stopped half-way goes on with `--resume` (tables already restored are left as they are).

The files hold every secret of the database (store keys, tokens): they are written readable by
their owner only; keep them so.

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
    # Queue drainers running now and their next runs (the worker starts them again).
    "drain_lane",
    "drain_schedule",
    # Inter-database calls in flight (SQL cannot write outboxes): a restored row calls again.
    "game_grant_outbox",
    "game_revoke_outbox",
    "idc_call",
    # Derived from the ledger: rebuilt after a restore (`admin_reindex_ledger`).
    "pending_purchase",
    "owned_purchase",
}
# Auto-increment primary keys that nothing refers to: inserted as 0, so the new database's sequence
# gives them ids (inserting the old ids would collide with ids it gives later).
FRESH_IDS = {"purchase_log": "id", "admin_audit": "id", "alert_log": "id", "store_create_job": "id", "integration_dead": "id"}
# Tables paged by a column that changes while the backup runs (a player's last request): a row
# touched after its page was read shows up again in a later page. Kept once per key (the last
# copy read), or the restore would fail on a duplicate key.
MOVING = {"user_attribution": "key", "player_profile": "key"}
# Tables whose values the dry run does not print (secrets, channel addresses).
HIDDEN = {"credential", "alert_channel"}
# Rows per INSERT statement.
BATCH = 200
# Large tables read in windows of an indexed integer column (rows per `/sql` answer at most PAGE).
# (column, lowest, past the highest value): integer columns, so the windows fit their type.
MICROS, IDS, DAYS = (0, 2 ** 62), (0, 2 ** 62), (0, 2 ** 31 - 1)
PAGED = {
    "purchase": ("created_at_micros", *MICROS),
    "purchase_log": ("id", *IDS),
    "admin_audit": ("id", *IDS),
    "user_attribution": ("updated_at_micros", *MICROS),
    "player_profile": ("last_seen_micros", *MICROS),
    "steam_order": ("order_id", *IDS),
    "revenue_daily": ("day", *DAYS),
    "revenue_country_daily": ("day", *DAYS),
    "subscription_daily": ("day", *DAYS),
    "paywall_daily": ("day", *DAYS),
}
PAGE = 20_000
# Where a restore stopped (in the backup's directory).
PROGRESS = "restore.progress.json"


def write_private(path, data, indent=None):
    """JSON to a file only its owner may read (the backup holds every secret)."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=indent)


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


def read_pages(base, db, tok, name, schema):
    """Rows of a table through the module's `admin_backup_page` (an index walk), or None when the
    module has no such procedure (published before it) or does not page this table."""
    cols = column_names(schema)
    rows, start = [], -(2 ** 63)
    while True:
        try:
            out = call_raw(base, db, tok, "admin_backup_page", name, start, PAGE)
        except StdbError:
            return None
        if not isinstance(out, dict) or "error" in out:
            return None
        rows.extend([r.get(c) for c in cols] for r in out["rows"])
        if out.get("next") is None:
            return rows
        start = out["next"]


def read_table(base, db, tok, name):
    """(schema, rows) of a table: in one answer, or for the large ones in pages the module reads
    through an index (`/sql` would scan the whole table for every window), else in windows of
    their column in `PAGED`."""
    if name not in PAGED:
        result = sql(base, db, tok, f"SELECT * FROM {name}")[0]
        return result["schema"], result["rows"]
    column, lo, top = PAGED[name]
    total = sql(base, db, tok, f"SELECT COUNT(*) AS n FROM {name}")[0]["rows"][0][0]
    schema = sql(base, db, tok, f"SELECT * FROM {name} LIMIT 0")[0]["schema"]
    paged = read_pages(base, db, tok, name, schema)
    if paged is not None and len(paged) == total:
        return schema, paged
    if paged is not None:
        print(f"note     {name}: {len(paged)} of {total} rows through the index, reading it with /sql")
    rows, width = [], top - lo
    while len(rows) < total and lo < top:
        hi = min(lo + width, top)
        limit = "" if width == 1 else f" LIMIT {PAGE + 1}"
        got = sql(base, db, tok, f"SELECT * FROM {name} WHERE {column} >= {lo} AND {column} < {hi}{limit}")[0]["rows"]
        if len(got) > PAGE and width > 1:
            width //= 2
            continue
        rows.extend(got)
        lo = hi
        if len(got) < PAGE // 4:
            width *= 2
    return schema, rows


def target_columns(base, db, tok, name):
    """Column names and types of a table in the database restored into."""
    schema = sql(base, db, tok, f"SELECT * FROM {name} LIMIT 0")[0]["schema"]
    return column_names(schema), [e["algebraic_type"] for e in schema["elements"]]


def zero(algebraic_type):
    """The value a column the backup does not have gets."""
    (kind, _), = algebraic_type.items()
    if kind == "String":
        return ""
    if kind == "Bool":
        return False
    if kind in ("F32", "F64"):
        return 0.0
    if kind in ("I8", "U8", "I16", "U16", "I32", "U32", "I64", "U64", "I128", "U128", "I256", "U256"):
        return 0
    raise ValueError(f"no default for a new column of type {json.dumps(algebraic_type)}")


def converted(data, cols, types):
    """The backup's rows in the target table's columns: new columns get zero values, removed ones
    are dropped."""
    have = column_names(data["schema"])
    index = {c: i for i, c in enumerate(have)}
    rows = [[row[index[c]] if c in index else zero(t) for c, t in zip(cols, types)] for row in data["rows"]]
    elements = [{"name": {"some": c}, "algebraic_type": t} for c, t in zip(cols, types)]
    added = [c for c in cols if c not in index]
    dropped = [c for c in have if c not in cols]
    return {"schema": {"elements": elements}, "rows": rows}, added, dropped


def unique_rows(schema, rows, key):
    """Rows with distinct `key`: the last copy of each (read later, so newer)."""
    i = column_names(schema).index(key)
    last = {row[i]: row for row in rows}
    return list(last.values())


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
        schema, rows = read_table(base, a.database, tok, name)
        if name in MOVING:
            rows = unique_rows(schema, rows, MOVING[name])
        write_private(os.path.join(a.dir, f"{name}.json"), {"schema": schema, "rows": rows})
        manifest["tables"][name] = len(rows)
        print(f"backup   {name}: {len(rows)} rows")
    write_private(os.path.join(a.dir, "manifest.json"), manifest, indent=2)
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


def keep_target_admins(base, db, tok, data):
    """Adds the restored database's own admins (its owner above all) to the backup's `admin` rows:
    restoring into a database another identity owns would otherwise lock its owner out."""
    current = sql(base, db, tok, "SELECT * FROM admin")[0]
    cols, have = column_names(data["schema"]), column_names(current["schema"])
    # `/sql` gives an Identity as a list or object: compared as JSON.
    known = {json.dumps(row[cols.index("identity")]) for row in data["rows"]}
    for row in current["rows"]:
        if json.dumps(row[have.index("identity")]) not in known:
            data["rows"].append([row[have.index(c)] if c in have else None for c in cols])


def mark_done(a, progress, path, name):
    if a.dry_run:
        return
    progress["done"].append(name)
    write_private(path, progress)


def restore(a, base, tok):
    with open(os.path.join(a.dir, "manifest.json"), encoding="utf-8") as f:
        manifest = json.load(f)
    if manifest.get("format_version") != FORMAT_VERSION:
        sys.exit(f"backup format {manifest.get('format_version')} is not {FORMAT_VERSION}: restore it with the purser "
                 "version that made it")
    present = set(tables(base, a.database, tok))
    gone = [t for t in manifest["tables"] if t not in present]
    if gone:
        print(f"skipped  tables the new module no longer has: {', '.join(gone)}")
    def show(q):
        name = q.split()[2] if q.startswith(("INSERT INTO", "DELETE FROM")) else ""
        if name in HIDDEN and q.startswith("INSERT"):
            q = q[:q.index(" VALUES ")] + " VALUES <hidden>"
        print(q[:200] + ("..." if len(q) > 200 else ""))
    run = show if a.dry_run else (lambda q: sql(base, a.database, tok, q))
    progress_path = os.path.join(a.dir, PROGRESS)
    progress = {"done": [], "channel_ids": {}}
    if a.resume and os.path.exists(progress_path):
        with open(progress_path, encoding="utf-8") as f:
            progress = json.load(f)
    elif os.path.exists(progress_path) and not a.dry_run:
        os.remove(progress_path)

    # Alert channels first: they get new ids, and their secrets' scopes follow.
    order = sorted((t for t in manifest["tables"] if t in present), key=lambda t: (t != "alert_channel", t == "credential", t))
    channel_ids = progress["channel_ids"]
    for name in order:
        if name in progress["done"]:
            print(f"restore  {name}: already restored")
            continue
        with open(os.path.join(a.dir, f"{name}.json"), encoding="utf-8") as f:
            data = json.load(f)
        data, added, dropped = converted(data, *target_columns(base, a.database, tok, name))
        if added or dropped:
            print(f"convert  {name}: new columns {added or '-'}, removed {dropped or '-'}")
        if name == "admin":
            keep_target_admins(base, a.database, tok, data)
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
            mark_done(a, progress, progress_path, name)
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
        mark_done(a, progress, progress_path, name)
    if not a.dry_run:
        reindex(base, a.database, tok)
        os.remove(progress_path)
    print("done     the worker continues unfinished purchases within a minute (admin page: Overview)")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("action", choices=["backup", "restore"])
    p.add_argument("--server", default="maincloud", help="CLI server nickname or URL")
    p.add_argument("--database", required=True)
    p.add_argument("--dir", required=True)
    p.add_argument("--force", action="store_true", help="backup: write into a non-empty --dir")
    p.add_argument("--dry-run", action="store_true", help="restore: print the statements instead of running them")
    p.add_argument("--resume", action="store_true", help="restore: go on after the tables a stopped restore finished")
    a = p.parse_args()
    base = a.server if a.server.startswith("http") else server_url(a.server)
    try:
        (backup if a.action == "backup" else restore)(a, base, token())
    except StdbError as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
