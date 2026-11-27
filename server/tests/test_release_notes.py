"""tools/release_notes.py takes a release's notes from the changelogs, and every released section
of CHANGELOG.md starts the way docs/releases.md says."""
import os
import sys
import unittest

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
sys.path.insert(0, os.path.join(ROOT, "tools"))

import release_notes  # noqa: E402

CHANGELOG = """# Changelog

## Unreleased

- Not yet.

## 0.2.0 - 2026-11-20

**In short:** two.

### Added

- Two.

## 0.1.0 - 2026-11-10

**In short:** one.
"""


class SectionTest(unittest.TestCase):
    def test_the_section_up_to_the_next_release(self):
        self.assertEqual(release_notes.section(CHANGELOG, "0.2.0"), "**In short:** two.\n\n### Added\n\n- Two.")

    def test_the_last_section_runs_to_the_end(self):
        self.assertEqual(release_notes.section(CHANGELOG, "0.1.0"), "**In short:** one.")

    def test_a_missing_or_partial_version_has_none(self):
        self.assertIsNone(release_notes.section(CHANGELOG, "0.3.0"))
        self.assertIsNone(release_notes.section(CHANGELOG, "0.2"))
        self.assertIsNone(release_notes.section("## 0.1.0 - 2026-11-10\n\n## 0.0.9\n", "0.1.0"))

    def test_keep_a_changelog_headings(self):
        self.assertEqual(release_notes.section("## [0.1.0] - 2026-11-10\n\nOne.\n", "0.1.0"), "One.")

    def test_released_versions_leave_out_unreleased(self):
        self.assertEqual(release_notes.released_versions(CHANGELOG), ["0.2.0", "0.1.0"])


class RepositoryChangelogTest(unittest.TestCase):
    def setUp(self):
        with open(os.path.join(ROOT, release_notes.SERVER_CHANGELOG), encoding="utf-8") as f:
            self.text = f.read()

    def test_every_release_starts_with_in_short_and_upgrade(self):
        versions = release_notes.released_versions(self.text)
        self.assertTrue(versions)
        for v in versions:
            body = release_notes.section(self.text, v) or ""
            first = body.split("\n\n")[:2]
            self.assertTrue(first and first[0].startswith("**In short:** "), f"{v}: starts with **In short:**")
            self.assertTrue(len(first) > 1 and first[1].startswith("**Upgrade:** "), f"{v}: **Upgrade:** follows")

    def test_notes_of_the_latest_release_add_the_unity_package(self):
        latest = release_notes.released_versions(self.text)[0]
        text = release_notes.notes(latest, ROOT)
        self.assertTrue(text.startswith("**In short:** "))
        self.assertIn("## Unity package", text)

    def test_an_unknown_version_is_refused(self):
        with self.assertRaises(LookupError):
            release_notes.notes("99.0.0", ROOT)


if __name__ == "__main__":
    unittest.main()
