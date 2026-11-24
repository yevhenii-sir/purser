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
        s.add_app("com.example.a.ios", "ios", game_key="game-a")
        s.add_app("com.example.a", "android", game_key="game-a")
        s.add_app("com.example.off", "android", game_key="game-c", enabled=False)
        return s

    def test_all_ok(self):
        with self.servers() as s:
            code, out, err = self.run_check_all(s, "--jobs", "2")
        self.assertEqual(code, 0)
        checked = sorted(c[1] for c in s.calls if c[0] == "admin_check_app")
        self.assertEqual(checked, ["com.example.a", "com.example.a.ios", "com.example.b"])
        self.assertEqual([c for c in s.calls if c[0] == "sql"], [("sql", "SELECT * FROM game_app")])
        self.assertIn("1 disabled build(s) skipped", err)
        lines = out.splitlines()
        self.assertEqual(lines[0].split(), ["APP", "BUILD", "STORE", "RESULT", "OK", "WARN", "FAIL"])
        self.assertEqual([l.split()[1] for l in lines[1:4]], ["com.example.a", "com.example.a.ios", "com.example.b"])
        self.assertIn("3 build(s): 3 ok, 0 with warnings, 0 failed", out)

    def test_failures_and_errors_exit_1(self):
        with self.servers() as s:
            s.reports["com.example.a"] = report("com.example.a", "warn", ok=5, warn=1)
            s.reports["com.example.b"] = report("com.example.b", "fail", ok=4, fail=1, checks=[
                {"id": "secrets", "title": "Secrets", "level": "fail", "detail": "google_service_account_json is missing",
                 "fix": "Run setup_google.py"},
                {"id": "x", "title": "Other", "level": "warn", "detail": "meh", "fix": ""}])
            s.reports["com.example.a.ios"] = {"error": "[ERR_ADMIN_UNAUTHORIZED] no"}
            code, out, _ = self.run_check_all(s)
        self.assertEqual(code, 1)
        self.assertIn("3 build(s): 0 ok, 1 with warnings, 1 failed, 1 not checked", out)
        self.assertIn("  [FAIL] Secrets: google_service_account_json is missing\n         -> Run setup_google.py", out)
        self.assertIn("com.example.a.ios:\n  [ERROR] [ERR_ADMIN_UNAUTHORIZED] no", out)
        self.assertNotIn("meh", out)
        row = next(l for l in out.splitlines() if l.startswith("game-b"))
        self.assertEqual(row.split(), ["game-b", "com.example.b", "google", "FAIL", "4", "0", "1"])

    def test_json_and_admins_who_are_not_the_owner(self):
        with self.servers() as s:
            s.owner = False  # /sql refused: the builds come from admin_select
            code, out, _ = self.run_check_all(s, "--json", "--offline")
        self.assertEqual(code, 0)
        self.assertEqual(len(json.loads(out)), 3)
        options = {c[2] for c in s.calls if c[0] == "admin_check_app"}
        self.assertEqual(options, {json.dumps({"online": False, "apple_test": False})})


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
