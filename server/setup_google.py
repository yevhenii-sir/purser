"""Sets up Google Play for purser builds on the Google Cloud side, with your own gcloud login.

A thin wrapper over the admin procedure `admin_setup_google` (the admin page's Setup tab ->
"Set up with Google Cloud" does the same): it takes a short-lived access token from
`gcloud auth print-access-token` and hands it to the module, which enables the APIs, creates the
service account and its key (stored as google_service_account_json), the Pub/Sub topic Google Play
publishes to, a google_rtdn_secret per build and the RTDN push subscriptions. Every step is skipped
when it is already done, so the script is safe to run again. Then it prints what only Play Console
can do.

Usage (from server/, after `gcloud auth login` with a Google account that may change the Cloud
project, and `spacetime login` as the database owner or an admin with the Secrets role, or with
PURSER_TOKEN set):
    python setup_google.py --project my-cloud-project --dry-run    # print what would change
    python setup_google.py --project my-cloud-project              # every Google Play build
    python setup_google.py --project my-cloud-project --app-id com.example.mygame
    python setup_google.py --project my-cloud-project --new-key    # replace the service account key
    python setup_google.py --project my-cloud-project --rotate-rtdn-secret

The builds must exist already (admin page or seed.py). The access token is never printed or passed
on a command line; secret values never leave the database.
"""
import argparse
import json
import pathlib
import secrets
import shutil
import subprocess
import sys

import stdb

ROOT = pathlib.Path(__file__).resolve().parent
REGISTRY = ROOT / "admin-ui" / "src" / "lib" / "registry.json"
GOOGLE_STORE = "google"
PROCEDURE = "admin_setup_google"
MARK = {"created": "do  ", "updated": "do  ", "exists": "ok  ", "skipped": "skip",
        "would_create": "would", "would_update": "would", "failed": "FAIL"}


def gcloud_token():
    exe = shutil.which("gcloud")  # gcloud.cmd on Windows
    if not exe:
        sys.exit("the gcloud CLI is not installed: https://cloud.google.com/sdk/docs/install, then `gcloud auth login`")
    r = subprocess.run([exe, "auth", "print-access-token"], capture_output=True, text=True)
    tok = r.stdout.strip()
    if r.returncode != 0 or not tok:
        sys.exit(f"`gcloud auth print-access-token` failed (run `gcloud auth login`):\n{r.stderr.strip()}")
    return tok


def store_of(build, platforms):
    """The store id of a `game_app` row (empty `store` = the platform's default)."""
    return build.get("store") or (platforms.get(build.get("platform"), {}).get("default_store") or "")


def read(base, db, tok, table, column="", value=""):
    """Rows through `admin_select`: works for every admin, not only the database owner."""
    query = {"table": table}
    if column:
        query["where"] = {"column": column, "equals": value}
    v = stdb.call(base, db, tok, "admin_select", json.dumps(query))
    if isinstance(v, dict) and "error" in v:
        sys.exit(f"admin_select {table} failed: {v['error']}")
    return v or []


def google_builds(base, db, tok, wanted):
    platforms = {p["id"]: p for p in json.loads(REGISTRY.read_text(encoding="utf-8"))["platforms"]}
    rows = {r["app_id"]: r for r in read(base, db, tok, "game_app")}
    for app_id in wanted:
        if app_id not in rows:
            sys.exit(f"no build {app_id} in the database: add it first (admin page -> Add an app, or seed.py)")
        if store_of(rows[app_id], platforms) != GOOGLE_STORE:
            sys.exit(f"{app_id} is not a Google Play build")
    ids = wanted or sorted(a for a, r in rows.items() if store_of(r, platforms) == GOOGLE_STORE)
    if not ids:
        sys.exit("no Google Play builds in the database: add one first (admin page -> Add an app, or seed.py)")
    return ids


def route_base(base, db, tok):
    """`public_base_url`, else this server's route (as `seed.py run_check` falls back)."""
    rows = read(base, db, tok, "server_state", "key", "public_base_url")
    return ((rows[0].get("value") if rows else "") or f"{base}/v1/database/{db}/route").rstrip("/")


def print_result(r):
    if r.get("service_account"):
        print(f"Service account: {r['service_account']}")
    topics = r.get("topic") or r.get("topics")
    if topics:
        print(f"Topic: {', '.join(topics) if isinstance(topics, list) else topics}")
    print()
    for s in r.get("steps") or []:
        action = s.get("action", "")
        line = f"  [{MARK.get(action, action)[:5]:5}] {s.get('step', '')}"
        if s.get("detail"):
            line += f": {s['detail']}"
        print(line)
    manual = r.get("manual") or []
    if manual:
        print("\nLeft to do (no API for these):")
        for i, m in enumerate(manual, 1):
            print(f"  {i}. {m}")


def main(argv=None):
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    cfg_path = ROOT / "spacetime.json"
    cfg = json.loads(cfg_path.read_text(encoding="utf-8")) if cfg_path.exists() else {}
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--server", default=cfg.get("server", "maincloud"))
    p.add_argument("--database", default=cfg.get("database"))
    p.add_argument("--project", required=True, help="Google Cloud project id (the one linked to Play Console)")
    p.add_argument("--app-id", action="append", default=[], help="a Google Play build (repeatable; default: all)")
    p.add_argument("--scope", help="where google_service_account_json goes (default: the builds' developer account, else default)")
    p.add_argument("--topic", default="play-billing", help="Pub/Sub topic name (default: play-billing)")
    p.add_argument("--new-key", action="store_true", help="create and store a new service account key")
    p.add_argument("--rotate-rtdn-secret", action="store_true", help="new google_rtdn_secret per build, push URL updated")
    p.add_argument("--dry-run", action="store_true", help="print what would change, change nothing")
    a = p.parse_args(argv)
    if not a.database or a.database.startswith("<"):
        sys.exit("No database: pass --database or copy spacetime.example.json to spacetime.json and set it")

    base, tok = stdb.server_url(a.server), stdb.token()
    app_ids = google_builds(base, a.database, tok, a.app_id)
    route = route_base(base, a.database, tok)
    print(f"Google Cloud project {a.project}; builds: {', '.join(app_ids)}; route: {route}")
    if a.dry_run:
        print("Dry run: nothing is changed.")

    request = {"project": a.project, "app_ids": app_ids, "access_token": gcloud_token(), "topic": a.topic,
               "route": route, "new_key": a.new_key, "rotate_rtdn_secret": a.rotate_rtdn_secret,
               # Random bytes for new google_rtdn_secret values (the module RNG is not fit for secrets).
               "entropy": secrets.token_hex(32),
               "dry_run": a.dry_run}
    if a.scope:
        request["scope"] = a.scope
    try:
        # The token travels in the request body only; errors carry the URL and the server's answer.
        r = stdb.call_raw(base, a.database, tok, PROCEDURE, json.dumps(request))
    except stdb.StdbError as e:
        sys.exit(f"{PROCEDURE} failed:\n{(e.body or str(e)).strip()}")
    finally:
        request.pop("access_token", None)
    if not isinstance(r, dict):
        sys.exit(f"{PROCEDURE}: unexpected answer {str(r)[:200]}")
    if "error" in r:
        sys.exit(f"{PROCEDURE} failed: {r['error']}")

    print_result(r)
    print("\nThen: python seed.py --check-only (or the app's Setup tab) should show \"Test notification received\".")
    print("If your seed files (credentials.json) hold google_service_account_json or google_rtdn_secret values,")
    print("replace them or remove those entries: seed.py without --check-only would upload the old values again.")
    if a.dry_run:
        print("\nDry run: nothing was changed.")
    sys.exit(0 if r.get("ok") else 1)


if __name__ == "__main__":
    main()
