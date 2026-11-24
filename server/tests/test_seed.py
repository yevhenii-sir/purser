"""Tests of seed.py's pure helpers (no server needed): `python -m unittest discover server/tests`."""
import hashlib
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import seed  # noqa: E402


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
