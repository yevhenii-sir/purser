"""Tests of seed.py (no server needed: fake_stdb.py stands in): `python -m unittest discover -s server/tests`."""
import contextlib
import hashlib
import io
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import seed  # noqa: E402
from fake_stdb import DB, FakeServer  # noqa: E402


def report(app_id, summary, ok=0, warn=0, fail=0, checks=()):
    return {"app_id": app_id, "summary": summary, "counts": {"ok": ok, "warn": warn, "fail": fail},
            "checks": list(checks), "urls": {}}


class CheckAllTest(unittest.TestCase):
    def run_check_all(self, server, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as e:
            seed.main(["--server", "local", "--database", DB, "--check-all", *args])
        return e.exception.code, out.getvalue(), err.getvalue()

    def servers(self):
        s = FakeServer()
        s.add_app("com.example.b", "android", game_key="game-b", account="studio")
        s.add_app("com.example.a", "ios", game_key="game-a")
        s.add_app("com.example.a", "android", game_key="game-a")
        s.add_app("com.example.off", "android", game_key="game-c", enabled=False)
        return s

    def test_all_ok(self):
        with self.servers() as s:
            code, out, err = self.run_check_all(s, "--jobs", "2")
        self.assertEqual(code, 0)
        checked = sorted(c[1] for c in s.calls if c[0] == "admin_check_app")
        self.assertEqual(checked, ["apple:com.example.a", "google:com.example.a", "google:com.example.b"])
        self.assertEqual([c for c in s.calls if c[0] == "sql"], [("sql", "SELECT * FROM game_app")])
        self.assertIn("1 disabled build(s) skipped", err)
        lines = out.splitlines()
        self.assertEqual(lines[0].split(), ["APP", "BUILD", "STORE", "RESULT", "OK", "WARN", "FAIL"])
        self.assertEqual([l.split()[1] for l in lines[1:4]], ["apple:com.example.a", "google:com.example.a",
                                                               "google:com.example.b"])
        self.assertIn("3 build(s): 3 ok, 0 with warnings, 0 failed", out)

    def test_failures_and_errors_exit_1(self):
        with self.servers() as s:
            s.reports["google:com.example.a"] = report("google:com.example.a", "warn", ok=5, warn=1)
            s.reports["google:com.example.b"] = report("google:com.example.b", "fail", ok=4, fail=1, checks=[
                {"id": "secrets", "title": "Secrets", "level": "fail", "detail": "google_service_account_json is missing",
                 "fix": "Run setup_google.py"},
                {"id": "x", "title": "Other", "level": "warn", "detail": "meh", "fix": ""}])
            s.reports["apple:com.example.a"] = {"error": "[ERR_ADMIN_UNAUTHORIZED] no"}
            code, out, _ = self.run_check_all(s)
        self.assertEqual(code, 1)
        self.assertIn("3 build(s): 0 ok, 1 with warnings, 1 failed, 1 not checked", out)
        self.assertIn("  [FAIL] Secrets: google_service_account_json is missing\n         -> Run setup_google.py", out)
        self.assertIn("apple:com.example.a:\n  [ERROR] [ERR_ADMIN_UNAUTHORIZED] no", out)
        self.assertNotIn("meh", out)
        row = next(l for l in out.splitlines() if l.startswith("game-b"))
        self.assertEqual(row.split(), ["game-b", "google:com.example.b", "google", "FAIL", "4", "0", "1"])

    def test_json_and_admins_who_are_not_the_owner(self):
        with self.servers() as s:
            s.owner = False  # /sql refused: the builds come from admin_select
            code, out, _ = self.run_check_all(s, "--json", "--offline")
        self.assertEqual(code, 0)
        self.assertEqual(len(json.loads(out)), 3)
        options = {c[2] for c in s.calls if c[0] == "admin_check_app"}
        self.assertEqual(options, {json.dumps({"online": False, "apple_test": False})})


class BuildKeyTest(unittest.TestCase):
    EXAMPLE = seed.ROOT / "seed.example"

    def test_app_key(self):
        self.assertEqual(seed.app_key({"store_app_id": "com.example.mygame", "platform": "ios"}), "apple:com.example.mygame")
        self.assertEqual(seed.app_key({"store_app_id": "web-1", "platform": "web", "store": "stripe"}), "stripe:web-1")
        for bad in ({"store_app_id": "web-1", "platform": "web"}, {"app_id": "com.example.mygame", "platform": "android"}):
            with self.assertRaises(SystemExit, msg=bad):
                seed.app_key(bad)

    def test_the_example_files_name_builds_by_key(self):
        load = lambda name: json.loads((self.EXAMPLE / name).read_text(encoding="utf-8"))
        apps = load("apps.json")
        keys = {seed.app_key(a) for a in apps}
        # One package, one build per store.
        self.assertEqual(keys, {"google:com.example.mygame", "apple:com.example.mygame"})
        self.assertTrue(all("app_id" not in a for a in apps))
        self.assertEqual({p["app_id"] for p in load("products.json")}, keys)
        self.assertLessEqual({i["app_id"] for i in load("integrations.json")}, keys)
        scopes = keys | {a["account"] for a in apps} | {"default"}
        self.assertLessEqual({c["scope"] for c in load("credentials.json")}, scopes)
        self.assertLessEqual(set(load("settings.json")["store_commission_by_app"]), keys)

    def test_check_runs_for_the_keys_of_apps_json(self):
        out = io.StringIO()
        with FakeServer() as s, contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as e:
            seed.main(["--server", "local", "--database", DB, "--dir", "seed.example", "--check-only", "--offline"])
        self.assertEqual(e.exception.code, 0, out.getvalue())
        self.assertEqual([c[1] for c in s.calls if c[0] == "admin_check_app"],
                         ["google:com.example.mygame", "apple:com.example.mygame"])

    def test_unity_config_names_builds_by_their_store_id(self):
        builds = [{"app_id": "google:com.example.mygame", "store_app_id": "com.example.mygame", "store": "google"},
                  {"app_id": "apple:com.example.mygame", "store_app_id": "com.example.mygame", "store": "apple"}]
        products = [{"app_id": "google:com.example.mygame", "product_id": "gems", "kind": "consumable"},
                    {"app_id": "apple:com.example.mygame", "product_id": "com.example.gems", "catalog_id": "gems",
                     "kind": "consumable"}]
        config = json.loads(seed.unity_config("https://stdb.example.com/route", builds, products))
        self.assertEqual(config["builds"], [{"store": "google", "appId": "com.example.mygame"},
                                            {"store": "apple", "appId": "com.example.mygame"}])
        self.assertEqual(config["products"], [{"id": "gems", "type": "Consumable", "storeIds": {"apple": "com.example.gems"}}])


class ServerKeyTest(unittest.TestCase):
    def test_new_key_has_the_admin_page_format(self):
        key_id, key, key_hash = seed.new_server_key()
        # The module accepts key ids of 8-24 characters a-z 0-9 (server_api.rs, ERR_BAD_KEY).
        self.assertRegex(key_id, r"^[0-9a-f]{12}$")
        self.assertTrue(key.startswith(f"psk_{key_id}_"))
        self.assertRegex(key.split("_", 2)[2], r"^[A-Za-z0-9_-]{43}$")
        self.assertEqual(key_hash, hashlib.sha256(key.encode()).hexdigest())
        self.assertNotEqual(key, seed.new_server_key()[1])

    def test_scopes(self):
        self.assertEqual(seed.server_key_scopes("read"), "read")
        self.assertEqual(seed.server_key_scopes(" read, grant ,read"), "read,grant")
        for bad in ("", "write", "read,admin", " , "):
            with self.assertRaises(SystemExit, msg=bad):
                seed.server_key_scopes(bad)


if __name__ == "__main__":
    unittest.main()
