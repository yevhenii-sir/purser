"""Copies a purser database to another one (moving to another server): a backup of the old
database restored into the new one (see backup.py).

Usage (from server/, as the owner of both databases):
    python migrate.py --from maincloud --from-db old-billing --to myserver --to-db new-billing
    python migrate.py ... --dry-run        # back up, then print what would be written

Tokens: `PURSER_TOKEN` (or the `spacetime login` one) is used for both servers. When the old
database is owned by another identity, put that identity's token in `PURSER_FROM_TOKEN`: it is
sent only to the --from server.

Before running:
  * the new database is freshly published with the same purser version as the old one: every
    copied table of it is replaced;
  * for a self-hosted server behind nginx, the SSH tunnel is open (see docs/self-hosting.md), since
    /sql is reachable only through it;
  * switch the clients and store notifications to the new server right after: purchases the old
    one records later are not copied (run the script again before the switch to start over).

The backup stays in --dir (default `backups/migrate-<from-db>`).
"""
import argparse
import os
import sys

import backup
from stdb import StdbError, server_url, token


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0],
                                epilog="Environment: PURSER_TOKEN (both servers, default: the `spacetime login` "
                                       "token), PURSER_FROM_TOKEN (only the --from server, default: the same token).")
    p.add_argument("--from", dest="src", required=True, help="CLI server nickname (or URL) of the old database")
    p.add_argument("--from-db", required=True)
    p.add_argument("--to", dest="dst", required=True, help="CLI server nickname (or URL) of the new database")
    p.add_argument("--to-db", required=True)
    p.add_argument("--dir", help="where the backup is written (default backups/migrate-<from-db>)")
    p.add_argument("--dry-run", action="store_true")
    a = p.parse_args()

    tok = token()
    src_tok = os.environ.get("PURSER_FROM_TOKEN", "").strip() or tok
    url = lambda s: s if s.startswith("http") else server_url(s)
    folder = a.dir or os.path.join("backups", f"migrate-{a.from_db}")
    try:
        backup.backup(argparse.Namespace(database=a.from_db, dir=folder, force=True), url(a.src), src_tok)
        backup.restore(argparse.Namespace(database=a.to_db, dir=folder, dry_run=a.dry_run, resume=False), url(a.dst), tok)
    except StdbError as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
