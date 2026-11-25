"""backup.py knows every table of the module that needs care: scheduled tables (their rows are
the host's timers: restoring them would fire old jobs) are skipped, and auto-increment keys that
nothing refers to get new ids (inserting old ids collides with the ids the new database gives)."""
import glob
import os
import re
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import backup  # noqa: E402

SRC = os.path.join(os.path.dirname(__file__), "..", "spacetimedb", "src")
# Auto-increment tables whose ids other rows refer to: restored with their ids on purpose.
KEPT_IDS = {
    "alert_channel",  # credentials are scoped by `alert-<id>`: restore renumbers them itself
    "idc_call", "game_grant_outbox", "game_revoke_outbox",  # skipped (outboxes, calls in flight)
}


def tables():
    """(name, scheduled, has auto_inc) of every `#[spacetimedb::table]` in the module."""
    out = []
    for path in glob.glob(os.path.join(SRC, "*.rs")):
        text = open(path, encoding="utf-8").read()
        for m in re.finditer(r"#\[spacetimedb::table\((.*?)\)\]\s*(?:#\[[^\]]*\]\s*)*pub struct \w+ \{(.*?)\n\}", text, re.S):
            name = re.search(r"accessor = (\w+)", m.group(1)).group(1)
            out.append((name, "scheduled(" in m.group(1), "#[auto_inc]" in m.group(2)))
    return out


class BackupTables(unittest.TestCase):
    def test_the_module_has_tables(self):
        self.assertGreater(len(tables()), 20)

    def test_scheduled_tables_are_not_restored(self):
        missing = [n for n, scheduled, _ in tables() if scheduled and n not in backup.SKIP]
        self.assertEqual(missing, [], "add them to SKIP in backup.py")

    def test_auto_increment_ids_are_given_by_the_new_database(self):
        missing = [n for n, _, auto in tables()
                   if auto and n not in backup.FRESH_IDS and n not in backup.SKIP and n not in KEPT_IDS]
        self.assertEqual(missing, [], "add them to FRESH_IDS in backup.py (or SKIP)")


if __name__ == "__main__":
    unittest.main()
