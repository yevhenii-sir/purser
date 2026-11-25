"""Says whether a new module can be published over a database in place, before publishing it.

    python publish_check.py --server maincloud --database <your-billing-db> --wasm purser.wasm

SpacetimeDB plans the migration from the database's current module to the new one (the
`pre_publish` endpoint `spacetime publish` asks first) and this prints the plan. Exit code 0: it
migrates in place, publish normally. Exit code 2: it does not (a table changed in a way SpacetimeDB
cannot migrate): back up with `backup.py backup`, publish with `--delete-data`, restore. Nothing is
published or changed. The token must be the database owner's: `PURSER_TOKEN`, else the
`spacetime login` one.
"""
import argparse
import json
import sys

from stdb import StdbError, request, server_url, token


def plan(base, db, tok, wasm):
    """The server's answer: `{"AutoMigrate": {...}}` or `{"ManualMigrate": {...}}`."""
    out = request("POST", f"{base}/v1/database/{db}/pre_publish?style=NoColor", tok, wasm, "application/octet-stream", timeout=300)
    return json.loads(out)


def verdict(answer):
    """(exit code, text) for the server's answer."""
    if "AutoMigrate" in answer:
        a = answer["AutoMigrate"]
        lines = ["Publishes in place (no --delete-data)."]
        if a.get("break_clients"):
            lines.append("It disconnects connected clients (a column was added, for example): publish with --break-clients.")
        if a.get("major_version_upgrade"):
            lines.append("It upgrades the database to a new major SpacetimeDB version.")
        lines.append("")
        lines.append(str(a.get("migrate_plan", "")).rstrip() or "(no schema changes)")
        return 0, "\n".join(lines)
    if "ManualMigrate" in answer:
        m = answer["ManualMigrate"]
        return 2, "\n".join([
            "Does NOT publish in place: " + str(m.get("reason", "")).strip(),
            "",
            "Back up, publish with --delete-data, restore:",
            "    python backup.py backup  --database <db> --dir backups/<date>",
            "    spacetime publish --delete-data -b purser.wasm <db>",
            "    python backup.py restore --database <db> --dir backups/<date>",
        ])
    return 1, f"Unexpected answer: {json.dumps(answer)[:500]}"


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--server", default="maincloud", help="a `spacetime server` nickname or a URL")
    p.add_argument("--database", required=True)
    p.add_argument("--wasm", required=True, help="the module to publish (purser.wasm of the release)")
    a = p.parse_args()
    base = server_url(a.server)
    with open(a.wasm, "rb") as f:
        wasm = f.read()
    try:
        answer = plan(base, a.database, token(), wasm)
    except StdbError as e:
        sys.exit(f"pre_publish failed: {e}")
    code, text = verdict(answer)
    # The plan draws with box characters a legacy console code page may not have.
    sys.stdout.reconfigure(errors="replace")
    print(text)
    sys.exit(code)


if __name__ == "__main__":
    main()
