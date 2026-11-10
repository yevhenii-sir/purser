"""Copies the purchase ledger from one purser database to another (moving to another server).

The new server then knows every purchase that was already granted: replayed purchase tokens are not
granted twice and `/entitlements` works without waiting for clients to restore.

Usage (from server/, logged in with `spacetime login` as the owner of both databases):
    python migrate.py --from maincloud --from-db old-billing --to myserver --to-db new-billing
    python migrate.py ... --dry-run        # only export and count

Before running:
  * the target database is published and its apps are configured (python seed.py --server ...);
  * for a self-hosted server behind nginx, the SSH tunnel is open (see docs/self-hosting.md), since
    /sql and /call of admin reducers are reachable only through it.
Rows that already exist on the target are skipped, so the script can be run again after the
switch to pick up purchases the old server recorded in the meantime. Revenue aggregates are
rebuilt by the target's worker once the imports have stopped for 5 minutes (in batches; the admin
page shows the revenue as rebuilding until then).
"""
import argparse
import json

from stdb import post, select, server_url, token

# Rows per admin_import_purchases call (the reducer takes at most 500).
BATCH = 500


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--from", dest="src", required=True, help="CLI server nickname of the old database")
    p.add_argument("--from-db", required=True)
    p.add_argument("--to", dest="dst", required=True, help="CLI server nickname of the new database")
    p.add_argument("--to-db", required=True)
    p.add_argument("--dry-run", action="store_true")
    a = p.parse_args()

    tok = token()
    src, dst = server_url(a.src), server_url(a.dst)
    rows = select(src, a.from_db, tok, "SELECT * FROM purchase")
    pending = sum(1 for r in rows if r["next_attempt_at_micros"] > 0)
    print(f"export   {len(rows)} purchases from {a.src}/{a.from_db}, {pending} with unfinished work")
    if pending:
        print("         unfinished rows are continued by the new server's worker; the game must be "
              "idempotent by transaction_id, since the old server may finish them too")
    if a.dry_run:
        return

    for i in range(0, len(rows), BATCH):
        batch = rows[i:i + BATCH]
        post(f"{dst}/v1/database/{a.to_db}/call/admin_import_purchases", tok,
             json.dumps([json.dumps(batch)]), "application/json")
        print(f"import   {i + len(batch)}/{len(rows)}")
    total = len(select(dst, a.to_db, tok, "SELECT id FROM purchase"))
    print(f"done     {a.dst}/{a.to_db} now has {total} purchases")
    print("         revenue is rebuilt by the worker about 5 minutes after the last import")


if __name__ == "__main__":
    main()
