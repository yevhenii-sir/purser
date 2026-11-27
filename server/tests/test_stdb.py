"""Tests of stdb.py's build keys (no server needed): `python -m unittest discover -s server/tests`."""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import stdb  # noqa: E402

REGISTRY = os.path.join(os.path.dirname(__file__), "..", "admin-ui", "src", "lib", "registry.json")


class BuildKeyTest(unittest.TestCase):
    def test_the_platform_default_store(self):
        self.assertEqual(stdb.build_key("android", "com.example.mygame"), "google:com.example.mygame")
        self.assertEqual(stdb.build_key("ios", "com.example.mygame"), "apple:com.example.mygame")
        self.assertEqual(stdb.build_key("amazon", "com.example.mygame"), "amazon:com.example.mygame")
        self.assertEqual(stdb.build_key("steam", "480"), "steam:480")

    def test_a_named_store(self):
        self.assertEqual(stdb.build_key("android", "com.example.mygame", "samsung"), "samsung:com.example.mygame")
        self.assertEqual(stdb.build_key("web", "com.example.mygame", "stripe"), "stripe:com.example.mygame")
        self.assertEqual(stdb.build_store("ios", "xsolla"), "xsolla")

    def test_what_the_server_refuses(self):
        cases = [
            (("web", "com.example.web"), "a web build needs a store: stripe or xsolla or telegram"),
            (("android", "com.example.mygame", "apple"), "store apple is not available on android (use google or "),
            (("windows", "com.example.mygame"), "platform must be one of android"),
            (("android", ""), "store_app_id is required"),
        ]
        for args, message in cases:
            with self.assertRaises(ValueError, msg=args) as e:
                stdb.build_key(*args)
            self.assertTrue(str(e.exception).startswith(message), str(e.exception))

    def test_the_table_is_the_registry(self):
        """PLATFORM_STORES and DEFAULT_STORE mirror `registry::PLATFORMS` (its snapshot for the admin page)."""
        with open(REGISTRY, encoding="utf-8") as f:
            platforms = json.load(f)["platforms"]
        self.assertEqual({p["id"]: tuple(p["stores"]) for p in platforms}, stdb.PLATFORM_STORES)
        self.assertEqual({p["id"]: p["default_store"] for p in platforms if p["default_store"]}, stdb.DEFAULT_STORE)


if __name__ == "__main__":
    unittest.main()
