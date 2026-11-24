"""Tests of bulk_setup.py against an in-memory server (fake_stdb.py): `python -m unittest discover -s server/tests`."""
import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import bulk_setup  # noqa: E402
import seed  # noqa: E402
from fake_stdb import DB, FakeServer  # noqa: E402

STORES, PLATFORMS = seed.registry()


def run(*args):
    """bulk_setup.main with the test database -> (exit code, stdout, stderr)."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = bulk_setup.main(["--server", "local", "--database", DB, *args])
    return code, out.getvalue(), err.getvalue()


class Files:
    """Writes test files into a temporary folder."""

    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = pathlib.Path(self.tmp.name)

    def write(self, name, text):
        path = self.dir / name
        path.write_text(text, encoding="utf-8")
        return str(path)

    def close(self):
        self.tmp.cleanup()


class ReadTest(unittest.TestCase):
    def test_csv_skips_comments_and_blank_lines_and_keeps_line_numbers(self):
        rows, errors = bulk_setup.csv_rows("﻿# builds\nApp_ID , platform\n\n com.example.a , android\n"
                                           "com.example.b,ios,\n", "apps.csv")
        self.assertEqual(errors, [])
        self.assertEqual(rows, [("apps.csv:4", {"app_id": "com.example.a", "platform": "android"}),
                                ("apps.csv:5", {"app_id": "com.example.b", "platform": "ios"})])

    def test_csv_errors(self):
        _, errors = bulk_setup.csv_rows("app_id,platform\ncom.example.a,android,extra\n", "apps.csv")
        self.assertEqual(errors, ["apps.csv:2: 3 cells, the header has 2"])
        _, errors = bulk_setup.csv_rows("app_id,app_id\n", "apps.csv")
        self.assertIn("repeated column", errors[0])

    def test_json_rows(self):
        rows, errors = bulk_setup.json_rows('[{"app_id": "a", "enabled": false, "price_micros": 990000}, 3]', "x.json")
        self.assertEqual(rows, [("x.json[0]", {"app_id": "a", "enabled": "false", "price_micros": "990000"})])
        self.assertEqual(errors, ["x.json[1]: expected an object"])
        self.assertIn("expected a list", bulk_setup.json_rows("{}", "x.json")[1][0])


class CheckAppsTest(unittest.TestCase):
    def check(self, *rows):
        return bulk_setup.check_apps([(f"apps.csv:{i + 2}", r) for i, r in enumerate(rows)], STORES, PLATFORMS)

    def test_store_defaults_to_the_platform_store(self):
        apps, errors = self.check({"app_id": "com.example.a", "platform": "android", "store": "", "allow_sandbox": "no"})
        self.assertEqual(errors, [])
        self.assertEqual(apps["com.example.a"][1], {"app_id": "com.example.a", "platform": "android", "store": "google",
                                                     "allow_sandbox": False})

    def test_errors_name_the_line(self):
        _, errors = self.check(
            {"app_id": "com.example.a", "platform": "android", "delivery": "none"},
            {"app_id": "com.example.a", "platform": "android"},
            {"app_id": "web.a", "platform": "web"},
            {"app_id": "x", "platform": "android", "store": "apple"},
            {"app_id": "y", "platform": "windows"},
            {"app_id": "a/b", "platform": "ios", "delivery": "email", "enabled": "maybe", "colour": "red"},
            {"app_id": "z", "platform": "android", "apple_refund_mode": "inform", "webhook_url": "http://x"},
        )
        self.assertEqual(errors[0], "apps.csv:3: app_id com.example.a is already on apps.csv:2")
        self.assertIn("apps.csv:4: a web build needs a store: stripe or xsolla", errors)
        self.assertTrue(any(e.startswith("apps.csv:5: store apple is not available on android (use google") for e in errors))
        self.assertTrue(any(e.startswith("apps.csv:6: platform must be one of android") for e in errors))
        line7 = [e for e in errors if e.startswith("apps.csv:7:")]
        self.assertEqual(len(line7), 4, line7)  # unknown column, enabled, app_id, delivery
        self.assertIn("apps.csv:8: apple_refund_mode is for App Store builds", errors)
        self.assertIn("apps.csv:8: webhook_url must start with https://", errors)


class CheckProductsTest(unittest.TestCase):
    def check(self, *rows):
        return bulk_setup.check_products([(f"products.csv:{i + 2}", r) for i, r in enumerate(rows)])

    def test_price_and_normalization(self):
        products, errors = self.check({"app_id": "a", "product_id": "vip", "kind": "subscription", "price": "4.99",
                                       "currency": "usd", "period": "p1m", "access": " premium, no_ads,premium"})
        self.assertEqual(errors, [])
        self.assertEqual(products[("a", "vip")][1], {"app_id": "a", "product_id": "vip", "kind": "subscription",
                                                     "price_micros": 4990000, "currency": "USD", "period": "P1M",
                                                     "access": "premium,no_ads"})
        self.assertEqual([bulk_setup.price_micros(x) for x in ("0.99", "10", "0.0000001", "-1", "abc", "NaN", "inf")],
                         [990000, 10000000, None, None, None, None, None])

    def test_errors_name_the_line(self):
        _, errors = self.check(
            {"app_id": "a", "product_id": "gems", "kind": "consumable"},
            {"app_id": "a", "product_id": "gems", "kind": "gem"},
            {"app_id": "a", "product_id": "has space", "kind": "consumable"},
            {"app_id": "a", "product_id": "p", "kind": "consumable", "price": "1", "price_micros": "1"},
            {"app_id": "a", "product_id": "q", "kind": "consumable", "period": "P1M", "currency": "dollars"},
            {"product_id": "r", "catalog_id": "bad id!"},
        )
        self.assertIn("products.csv:3: a gems is already on products.csv:2", errors)
        self.assertIn("products.csv:3: kind must be consumable, non_consumable, subscription, not 'gem'", errors)
        self.assertTrue(any(e.startswith("products.csv:4: product_id 'has space'") for e in errors))
        self.assertIn("products.csv:5: set price or price_micros, not both", errors)
        self.assertIn("products.csv:6: period is for subscriptions", errors)
        self.assertIn("products.csv:6: 'DOLLARS' is not a 3-letter currency code", errors)
        self.assertIn("products.csv:7: app_id is required", errors)
        self.assertTrue(any(e.startswith("products.csv:7: catalog_id 'bad id!'") for e in errors))


class BulkSetupTest(unittest.TestCase):
    def setUp(self):
        self.files = Files()
        self.server = FakeServer().__enter__()
        self.server.add_app("com.example.old", "android", game_key="old-game", account="studio",
                            delivery="webhook", webhook_url="https://game.example.com/billing")
        self.server.add_product("com.example.old", "gems_100", "consumable", price_micros=990000, currency="USD")

    def tearDown(self):
        self.server.__exit__(None, None, None)
        self.files.close()

    def apps_csv(self, *lines):
        return self.files.write("apps.csv", "game_key,platform,store,app_id,account,delivery\n" + "\n".join(lines) + "\n")

    def products_csv(self, *lines):
        return self.files.write("products.csv", "app_id,product_id,kind,catalog_id\n" + "\n".join(lines) + "\n")

    def test_invalid_files_call_nothing(self):
        apps = self.apps_csv("my-game,android,,com.example.a,studio,none", "my-game,android,,com.example.a,studio,none")
        products = self.products_csv("com.example.a,gems,gem,")
        code, out, err = run("--apps", apps, "--products", products)
        self.assertEqual(code, 1)
        self.assertEqual(self.server.calls, [])
        self.assertIn("apps.csv:3: app_id com.example.a is already on apps.csv:2", err)
        self.assertIn("products.csv:2: kind must be", err)
        self.assertIn("nothing was written", err)

    def test_server_dependent_errors_write_nothing(self):
        apps = self.apps_csv("my-game,ios,,com.example.ios,studio,")  # a new build without delivery
        products = self.products_csv("com.example.nowhere,gems,consumable,", "com.example.old,gems_200,consumable,gems_100")
        code, _, err = run("--apps", apps, "--products", products)
        self.assertEqual(code, 1)
        self.assertEqual(self.server.writes(), [])
        self.assertIn("apps.csv:2: a new build needs delivery", err)
        self.assertIn("products.csv:2: no build com.example.nowhere in the apps file or on the server", err)
        self.assertIn("products.csv:3: com.example.old has the catalog id gems_100 on gems_100 too", err)

    def test_dry_run_prints_the_diff_and_writes_nothing(self):
        apps = self.apps_csv("my-game,ios,,com.example.ios,studio,none",
                             "old-game,android,,com.example.old,,none")
        products = self.products_csv("com.example.ios,gems_100,consumable,",
                                     "com.example.old,gems_100,,",
                                     "com.example.old,no_ads,non_consumable,remove_ads")
        code, out, _ = run("--apps", apps, "--products", products, "--dry-run")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.server.writes(), [])
        self.assertIn("Builds: 1 new, 1 changed, 0 unchanged", out)
        self.assertIn("new      com.example.ios (ios/apple, app my-game, account studio, delivery none)", out)
        self.assertIn("changed  com.example.old: delivery 'webhook' -> 'none'", out)
        self.assertIn("Products: 2 new, 0 changed, 1 unchanged", out)
        self.assertIn("new      com.example.old no_ads (non_consumable, no price, catalog id remove_ads)", out)
        self.assertIn("Dry run: nothing was written.", out)

    def test_writes_then_a_second_run_changes_nothing(self):
        apps = self.apps_csv("my-game,ios,,com.example.ios,studio,none",
                             "my-game,android,google,com.example.and,studio,none",
                             "old-game,android,,com.example.old,,")
        products = self.files.write("products.csv", "app_id,product_id,kind,catalog_id,price,currency\n"
                                    "com.example.ios,com.example.gems100,consumable,gems_100,0.99,USD\n"
                                    "com.example.and,gems_100,consumable,,0.99,usd\n"
                                    "com.example.old,gems_100,,,1.09,EUR\n")
        code, out, _ = run("--apps", apps, "--products", products)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.server.apps["com.example.ios"]["store"], "apple")
        self.assertEqual(self.server.apps["com.example.ios"]["account"], "studio")
        # The empty delivery cell kept the stored webhook.
        self.assertEqual(self.server.apps["com.example.old"]["delivery"], "webhook")
        self.assertEqual(self.server.products["com.example.ios:com.example.gems100"]["catalog_id"], "gems_100")
        self.assertEqual(self.server.products["com.example.old:gems_100"]["price_micros"], 1090000)
        self.assertEqual(self.server.products["com.example.old:gems_100"]["kind"], "consumable")
        uploads = [c for c in self.server.calls if c[0] == "admin_upsert_products"]
        self.assertEqual(len(uploads), 1)
        self.assertEqual(len(json.loads(uploads[0][1])), 3)

        self.server.calls.clear()
        code, out, _ = run("--apps", apps, "--products", products)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.server.writes(), [])
        self.assertIn("Builds: 0 new, 0 changed, 3 unchanged", out)
        self.assertIn("Products: 0 new, 0 changed, 3 unchanged", out)

    def test_json_files(self):
        apps = self.files.write("apps.json", json.dumps([{"app_id": "com.example.j", "platform": "amazon",
                                                          "delivery": "none", "allow_sandbox": False}]))
        products = self.files.write("products.json", json.dumps([{"app_id": "com.example.j", "product_id": "gems",
                                                                  "kind": "consumable", "price_micros": 990000,
                                                                  "currency": "USD"}]))
        code, out, _ = run("--apps", apps, "--products", products)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.server.apps["com.example.j"]["allow_sandbox"], False)
        self.assertEqual(self.server.apps["com.example.j"]["store"], "amazon")
        self.assertEqual(self.server.products["com.example.j:gems"]["price_micros"], 990000)

    def test_a_refused_batch_is_sent_row_by_row(self):
        self.server.refuse_products = {"bad"}
        products = self.products_csv(*[f"com.example.old,p{i},consumable," for i in range(5)], "com.example.old,bad,consumable,")
        code, out, _ = run("--products", products, "--batch", "4")
        self.assertEqual(code, 1)
        self.assertIn("product com.example.old bad: [ERR_BAD_PRODUCT] bad: refused", out)
        self.assertEqual(sorted(k for k in self.server.products if ":p" in k),
                         [f"com.example.old:p{i}" for i in range(5)])
        sizes = [len(json.loads(c[1])) for c in self.server.calls if c[0] == "admin_upsert_products"]
        self.assertEqual(sizes, [4, 2, 1, 1])  # the second batch failed, then its two rows one by one

    def test_from_stores_adds_what_the_server_lacks(self):
        self.server.catalogs["com.example.old"] = {"app_id": "com.example.old", "store": "google", "products": [
            {"product_id": "gems_100", "kind": "consumable", "price_micros": 990000, "currency": "USD",
             "billing_period_days": 0, "status": "same", "local_kind": "consumable"},
            {"product_id": "gems_500", "kind": "consumable", "price_micros": 3990000, "currency": "USD",
             "billing_period_days": 0, "status": "new", "local_kind": ""},
            {"product_id": "no_ads", "kind": "consumable", "price_micros": 2990000, "currency": "USD",
             "billing_period_days": 0, "status": "new", "local_kind": ""},
            {"product_id": "vip", "kind": "subscription", "price_micros": 4990000, "currency": "USD",
             "billing_period_days": 30, "status": "new", "local_kind": ""},
            {"product_id": "vip_2m", "kind": "subscription", "price_micros": 0, "currency": "",
             "billing_period_days": 60, "status": "new", "local_kind": ""},
        ], "missing_in_store": []}
        # A file row wins over the store: Google Play does not say no_ads is non-consumable.
        products = self.files.write("products.csv", "app_id,product_id,kind\ncom.example.old,no_ads,non_consumable\n"
                                    "com.example.old,gems_500,\n")
        code, out, _ = run("--products", products, "--from-stores", "--dry-run")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.server.writes(), [])
        self.assertIn("Products: 4 new (2 from the stores), 0 changed, 0 unchanged", out)

        code, out, _ = run("--products", products, "--from-stores")
        self.assertEqual(code, 0, out)
        p = self.server.products
        self.assertEqual((p["com.example.old:no_ads"]["kind"], p["com.example.old:no_ads"]["price_micros"]),
                         ("non_consumable", 2990000))
        self.assertEqual((p["com.example.old:gems_500"]["kind"], p["com.example.old:gems_500"]["price_micros"]),
                         ("consumable", 3990000))
        self.assertEqual((p["com.example.old:vip"]["period"], p["com.example.old:vip"]["currency"]), ("P1M", "USD"))
        self.assertEqual(p["com.example.old:vip_2m"]["period"], "")  # no such period in purser
        self.assertEqual(p["com.example.old:gems_100"]["price_micros"], 990000)

    def test_from_stores_reports_a_build_it_could_not_list(self):
        self.server.add_app("com.example.ios", "ios", account="studio")
        self.server.catalogs["com.example.ios"] = {"error": "App Store Connect refused to list products"}
        self.server.catalogs["com.example.old"] = {"products": [], "missing_in_store": []}
        products = self.files.write("products.csv", "app_id,product_id,kind\ncom.example.ios,gems,\n")
        code, out, _ = run("--products", products, "--from-stores")
        self.assertEqual(code, 1)
        self.assertIn("store catalog of com.example.ios: App Store Connect refused to list products", out)
        self.assertIn("products.csv:2: no kind, and the build's store catalog was not listed", out)
        self.assertEqual(self.server.writes(), [])

    def test_from_stores_dry_run_skips_builds_not_created_yet(self):
        self.server.catalogs["com.example.old"] = {"products": [], "missing_in_store": []}
        apps = self.apps_csv("my-game,ios,,com.example.ios,studio,none", "old-game,android,,com.example.old,,")
        products = self.files.write("products.csv", "app_id,product_id,kind\ncom.example.ios,gems,\n")
        code, out, _ = run("--apps", apps, "--products", products, "--from-stores", "--dry-run")
        self.assertEqual(code, 0, out)
        self.assertIn("Store catalogs: 1 build(s), 1 new build(s) are listed after they are created", out)
        self.assertIn("later    1 product(s) without a kind", out)
        self.assertEqual([c[1] for c in self.server.calls if c[0] == "admin_list_store_products"], ["com.example.old"])


if __name__ == "__main__":
    unittest.main()
