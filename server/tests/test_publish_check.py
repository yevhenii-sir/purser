"""publish_check.py turns the server's migration plan into a verdict and an exit code."""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import publish_check  # noqa: E402


class Verdict(unittest.TestCase):
    def test_a_module_that_migrates_in_place(self):
        code, text = publish_check.verdict({"AutoMigrate": {"migrate_plan": "+ table drain_lane", "break_clients": False, "token": "x"}})
        self.assertEqual(code, 0)
        self.assertIn("in place", text)
        self.assertIn("drain_lane", text)

    def test_a_change_that_disconnects_clients_says_so(self):
        code, text = publish_check.verdict({"AutoMigrate": {"migrate_plan": "+ column", "break_clients": True}})
        self.assertEqual(code, 0)
        self.assertIn("--break-clients", text)

    def test_a_module_that_needs_delete_data(self):
        code, text = publish_check.verdict({"ManualMigrate": {"reason": "column purchase.app_id removed"}})
        self.assertEqual(code, 2)
        self.assertIn("--delete-data", text)
        self.assertIn("column purchase.app_id removed", text)

    def test_an_unknown_answer(self):
        self.assertEqual(publish_check.verdict({"Other": {}})[0], 1)


if __name__ == "__main__":
    unittest.main()
