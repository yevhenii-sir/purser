"""Tests of setup_google.py's batches (no server, no gcloud): `python -m unittest discover -s server/tests`."""
import contextlib
import io
import json
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import setup_google  # noqa: E402
from fake_stdb import DB, FakeServer  # noqa: E402


class BatchesTest(unittest.TestCase):
    def test_twenty_builds_per_call(self):
        builds = {f"com.example.g{i:02}": "studio" for i in range(45)}
        calls = setup_google.batches(builds)
        self.assertEqual([len(ids) for _, ids, _ in calls], [20, 20, 5])
        self.assertEqual([first for _, _, first in calls], [True, False, False])
        self.assertEqual([i for _, ids, _ in calls for i in ids], list(builds))

    def test_one_account_per_call_unless_a_scope_is_named(self):
        builds = {"a1": "studio", "b1": "publisher", "a2": "studio", "b2": "publisher", "c1": "default"}
        calls = setup_google.batches(builds)
        self.assertEqual(calls, [("studio", ["a1", "a2"], True), ("publisher", ["b1", "b2"], True),
                                 ("default", ["c1"], True)])
        self.assertEqual(setup_google.batches(builds, "shared"), [("shared", list(builds), True)])


class MainTest(unittest.TestCase):
    def run_main(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as e, \
                mock.patch.object(setup_google, "gcloud_token", return_value="ya29.test-token"):
            setup_google.main(["--server", "local", "--database", DB, "--project", "my-cloud-project", *args])
        return e.exception.code, out.getvalue()

    def test_many_builds_go_in_calls_of_twenty_on_one_topic(self):
        with FakeServer() as s:
            for i in range(25):
                s.add_app(f"com.example.g{i:02}", "android", account="studio")
            s.add_app("com.example.p", "android", account="publisher")
            s.add_app("com.example.ios", "ios", account="studio")
            code, out = self.run_main("--new-key", "--topic", "billing")
        self.assertEqual(code, 0, out)
        requests = [json.loads(c[1]) for c in s.calls if c[0] == "admin_setup_google"]
        self.assertEqual([len(r["app_ids"]) for r in requests], [20, 5, 1])
        self.assertEqual({r["topic"] for r in requests}, {"billing"})
        # A new key once per account; fresh entropy for every call.
        self.assertEqual([r["new_key"] for r in requests], [True, False, True])
        self.assertEqual(len({r["entropy"] for r in requests}), 3)
        self.assertNotIn("com.example.ios", [i for r in requests for i in r["app_ids"]])
        self.assertIn("== 2/3 (account studio)", out)
        self.assertEqual(out.count("Left to do"), 1)
        self.assertNotIn("ya29.test-token", out)

    def test_a_failed_call_stops_and_names_what_is_left(self):
        with FakeServer() as s:
            for i in range(30):
                s.add_app(f"com.example.g{i:02}", "android")
            calls = []

            def refuse_second(text):
                calls.append(text)
                if len(calls) == 2:
                    return {"error": "Pub/Sub API: permission denied"}
                return FakeServer.admin_setup_google(s, text)

            s.admin_setup_google = refuse_second
            code, out = self.run_main()
        self.assertEqual(code, 1)
        self.assertEqual(len(calls), 2)
        self.assertIn("Pub/Sub API: permission denied", out)
        self.assertIn("Not set up: com.example.g20, ", out)
        self.assertNotIn("com.example.g19,", out.split("Not set up:")[1])


if __name__ == "__main__":
    unittest.main()
