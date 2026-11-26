"""backup.py reads the large tables in pages of the module's `admin_backup_page`, and with `/sql`
windows when the module has no such procedure or its pages miss rows."""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import backup  # noqa: E402
from stdb import StdbError  # noqa: E402

SCHEMA = {"elements": [{"name": {"some": "id"}, "algebraic_type": {"String": []}},
                       {"name": {"some": "created_at_micros"}, "algebraic_type": {"I64": []}}]}


def fake_sql(rows):
    def sql(_base, _db, _tok, query):
        if query.startswith("SELECT COUNT(*)"):
            return [{"rows": [[len(rows)]]}]
        if query.endswith("LIMIT 0"):
            return [{"schema": SCHEMA, "rows": []}]
        return [{"schema": SCHEMA, "rows": [[r["id"], r["created_at_micros"]] for r in rows]}]
    return sql


class ReadTable(unittest.TestCase):
    rows = [{"id": f"google:{i}", "created_at_micros": 1000 + i} for i in range(5)]

    def test_large_tables_are_read_in_pages_of_the_module(self):
        pages = [{"rows": self.rows[:3], "next": 1003}, {"rows": self.rows[3:], "next": None}]
        calls = []

        def call_raw(_base, _db, _tok, name, table, start, limit):
            calls.append((name, table, start))
            return pages[len(calls) - 1]

        with mock.patch.object(backup, "sql", fake_sql(self.rows)), mock.patch.object(backup, "call_raw", call_raw):
            schema, rows = backup.read_table("http://x", "db", "tok", "purchase")
        self.assertEqual(rows, [[r["id"], r["created_at_micros"]] for r in self.rows], "in the schema's column order")
        self.assertEqual([c[2] for c in calls], [-(2 ** 63), 1003])
        self.assertEqual(schema, SCHEMA)

    def test_an_older_module_is_read_with_sql(self):
        def call_raw(*_args):
            raise StdbError("no such procedure")

        with mock.patch.object(backup, "sql", fake_sql(self.rows)), mock.patch.object(backup, "call_raw", call_raw):
            _, rows = backup.read_table("http://x", "db", "tok", "purchase")
        self.assertEqual(len(rows), 5)

    def test_pages_that_miss_rows_are_not_trusted(self):
        def call_raw(*_args):
            return {"rows": self.rows[:2], "next": None}

        with mock.patch.object(backup, "sql", fake_sql(self.rows)), mock.patch.object(backup, "call_raw", call_raw), \
                mock.patch("builtins.print"):
            _, rows = backup.read_table("http://x", "db", "tok", "purchase")
        self.assertEqual(len(rows), 5, "read again with /sql")


KEYED = {"elements": [{"name": {"some": "key"}, "algebraic_type": {"String": []}},
                      {"name": {"some": "last_seen_micros"}, "algebraic_type": {"I64": []}}]}


class Restore(unittest.TestCase):
    def test_a_row_touched_while_the_backup_ran_is_kept_once(self):
        rows = [["app/a", 1], ["app/b", 2], ["app/a", 3]]
        self.assertEqual(backup.unique_rows(KEYED, rows, "key"), [["app/a", 3], ["app/b", 2]])

    def test_the_restored_databases_own_admins_stay(self):
        schema = {"elements": [{"name": {"some": "identity"}, "algebraic_type": {"String": []}},
                               {"name": {"some": "protected"}, "algebraic_type": {"Bool": []}}]}
        data = {"schema": schema, "rows": [["0xold", True]]}
        current = [{"schema": schema, "rows": [["0xnew", True], ["0xold", True]]}]
        with mock.patch.object(backup, "sql", lambda *_: current):
            backup.keep_target_admins("http://x", "db", "tok", data)
        self.assertEqual(data["rows"], [["0xold", True], ["0xnew", True]])

    def test_ids_nothing_refers_to_are_given_by_the_new_database(self):
        self.assertIn("integration_dead", backup.FRESH_IDS)


class Convert(unittest.TestCase):
    def test_new_columns_get_zero_or_the_value_older_rows_had(self):
        data = {"schema": SCHEMA, "rows": [["google:1", 1000]]}
        cols = ["id", "created_at_micros", "quantity", "tax_micros", "reinstated"]
        types = [{"String": []}, {"I64": []}, {"U32": []}, {"I64": []}, {"Bool": []}]
        out, added, dropped = backup.converted(data, cols, types, table="purchase")
        self.assertEqual(out["rows"], [["google:1", 1000, 1, 0, False]], "an older purchase is one unit")
        self.assertEqual((added, dropped), (["quantity", "tax_micros", "reinstated"], []))
        other, _, _ = backup.converted(data, cols, types, table="something_else")
        self.assertEqual(other["rows"][0][2], 0)


class Inserts(unittest.TestCase):
    def test_wide_rows_go_in_inserts_short_enough_for_the_server(self):
        schema = {"elements": [{"name": {"some": "id"}, "algebraic_type": {"String": []}},
                               {"name": {"some": "details"}, "algebraic_type": {"String": []}}]}
        rows = [[f"p{i}", "x" * 900] for i in range(200)]
        queries = list(backup.insert_statements("purchase_log_like", {"schema": schema, "rows": rows}))
        self.assertGreater(len(queries), 1)
        self.assertTrue(all(len(q.encode()) <= 50_000 for q in queries), [len(q) for q in queries])
        self.assertEqual(sum(q.count("('p") for q in queries), 200, "every row once")


class Migrate(unittest.TestCase):
    def test_migrate_restores_without_resume(self):
        import migrate
        seen = {}
        with mock.patch.object(migrate.backup, "backup"),                 mock.patch.object(migrate.backup, "restore", lambda a, *_: seen.update(resume=a.resume)),                 mock.patch.object(sys, "argv", ["migrate.py", "--from", "http://127.0.0.1:1", "--from-db", "a",
                                                "--to", "http://127.0.0.1:2", "--to-db", "b"]),                 mock.patch.object(migrate, "token", lambda *_: "t", create=True):
            try:
                migrate.main()
            except SystemExit:
                pass
        self.assertEqual(seen.get("resume"), False)

    def run_switch(self, restore_fails=False, extra=()):
        import migrate
        from stdb import StdbError
        steps = []

        def restore(a, base, *_):
            steps.append(("restore", base, a.database))
            if restore_fails:
                raise StdbError("restore failed")

        def call(base, db, tok, name, *args):
            steps.append((name, base, db) + args)
            return {"ok": True} if name == "admin_set_move" else None

        argv = ["migrate.py", "--from", "http://127.0.0.1:1", "--from-db", "old", "--to", "http://127.0.0.1:2", "--to-db", "new", "--switch", *extra]
        with mock.patch.object(migrate.backup, "backup", lambda a, base, *_: steps.append(("backup", base, a.database))),                 mock.patch.object(migrate.backup, "restore", restore), mock.patch.object(migrate, "call", call),                 mock.patch.object(sys, "argv", argv), mock.patch.object(migrate, "token", lambda *_: "t"),                 mock.patch("builtins.print"):
            try:
                migrate.main()
            except SystemExit:
                pass
        return steps

    def test_switch_pauses_copies_marks_and_forwards(self):
        steps = self.run_switch()
        self.assertEqual([s[0] for s in steps], ["admin_set_move", "backup", "restore", "admin_mark_moved_in", "admin_set_move"])
        self.assertEqual(steps[0], ("admin_set_move", "http://127.0.0.1:1", "old", "paused", ""))
        self.assertEqual(steps[3][1:3], ("http://127.0.0.1:2", "new"))
        self.assertEqual(steps[4], ("admin_set_move", "http://127.0.0.1:1", "old", "forwarding", "http://127.0.0.1:2/v1/database/new/route"))
        to_url = self.run_switch(extra=["--to-url", "https://billing.example.com/"])
        self.assertEqual(to_url[-1][-1], "https://billing.example.com")

    def test_a_failed_copy_resumes_the_old_database(self):
        steps = self.run_switch(restore_fails=True)
        self.assertEqual([s[0] for s in steps], ["admin_set_move", "backup", "restore", "admin_set_move"])
        self.assertEqual(steps[-1], ("admin_set_move", "http://127.0.0.1:1", "old", "", ""))


if __name__ == "__main__":
    unittest.main()
