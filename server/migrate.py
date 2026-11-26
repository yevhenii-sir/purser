"""Copies a purser database to another one (moving to another server): a backup of the old
database restored into the new one (see backup.py and docs/moving-servers.md).

Usage (from server/, as the owner of both databases):
    python migrate.py --from maincloud --from-db old-billing --to myserver --to-db new-billing
    python migrate.py ... --dry-run        # back up, then print what would be written
    python migrate.py ... --switch         # the whole move: pause the old database, copy it, mark the
                                           # new one as moved in, then forward the old one to it
    python migrate.py ... --switch --to-url https://billing.example.com   # the address clients use

`--switch` first pauses the old database: its routes answer 503 with Retry-After (games and stores try
again, so nothing is lost) and its background jobs wait. Then the copy is complete, with nothing
recorded on the old server after it. The new database is marked as moved in (the Server page lists
what still points at the old address), and the old one forwards every route to the new one
(`--to-url`, default the new database's route base on its server) until nothing reaches it any more.
If the copy fails, the old database is resumed.

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
from stdb import StdbError, call, server_url, token


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
    p.add_argument("--switch", action="store_true", help="pause the old database, copy, mark the new one, forward the old one to it")
    p.add_argument("--to-url", help="with --switch: the new database's public route base the old one forwards to "
                                    "(default <the --to server>/v1/database/<to-db>/route)")
    a = p.parse_args()
    if a.switch and a.dry_run:
        p.error("--switch moves for real: run --dry-run without it first")

    tok = token()
    src_tok = os.environ.get("PURSER_FROM_TOKEN", "").strip() or tok
    url = lambda s: s if s.startswith("http") else server_url(s)
    folder = a.dir or os.path.join("backups", f"migrate-{a.from_db}")
    src, dst = url(a.src), url(a.dst)
    target = (a.to_url or f"{dst}/v1/database/{a.to_db}/route").rstrip("/")
    if a.switch:
        move(src, a.from_db, src_tok, "paused", "")
        print(f"{a.from_db}: paused (routes answer 503, jobs wait)")
    try:
        backup.backup(argparse.Namespace(database=a.from_db, dir=folder, force=True), src, src_tok)
        backup.restore(argparse.Namespace(database=a.to_db, dir=folder, dry_run=a.dry_run, resume=False), dst, tok)
    except StdbError as e:
        if a.switch:
            move(src, a.from_db, src_tok, "", "")
            print(f"{a.from_db}: resumed, nothing moved")
        sys.exit(str(e))
    if a.switch:
        call(dst, a.to_db, tok, "admin_mark_moved_in", f"{a.src} {a.from_db}")
        move(src, a.from_db, src_tok, "forwarding", target)
        print(f"{a.from_db}: forwards every route to {target}")
        print("Next: point the stable address (or the clients and store notifications) at the new server;")
        print("the old server's Server page counts what still reaches it, the new one lists what still points back.")


def move(base, db, tok, mode, target):
    """`admin_set_move` on a database; exits on refusal (the old one is left as it was)."""
    out = call(base, db, tok, "admin_set_move", mode, target)
    if isinstance(out, dict) and out.get("error"):
        sys.exit(f"{db}: {out['error']}")


if __name__ == "__main__":
    main()
