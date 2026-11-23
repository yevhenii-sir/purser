"""Tests of import_revenuecat.py's mapping (no server needed): `python -m unittest discover server/tests`."""
import csv
import gzip
import io
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import import_revenuecat as rc  # noqa: E402

HEADER = rc.REQUIRED + ["purchase_price_in_purchased_currency", "price_in_purchased_currency", "country", "ownership_type", "updated_at"]
NOW = rc.micros("2026-11-20 00:00:00")
KINDS = {"vip_monthly": "subscription", "no_ads": "non_consumable", "gems": "consumable"}


def row(**kw):
    r = {
        "store": "app_store", "store_transaction_id": "2000001", "original_store_transaction_id": "2000001",
        "product_identifier": "vip_monthly", "rc_original_app_user_id": "player-1", "start_time": "2026-10-01 10:00:00",
        "end_time": "2026-12-01 10:00:00", "is_auto_renewable": "true", "is_trial_period": "false", "is_sandbox": "false",
        "refunded_at": "", "purchased_currency": "usd", "renewal_number": "1",
        "purchase_price_in_purchased_currency": "9.99", "price_in_purchased_currency": "9.99", "country": "us",
        "ownership_type": "PURCHASED", "updated_at": "2026-10-01 10:00:05",
    }
    r.update(kw)
    return r


class MapTest(unittest.TestCase):
    def test_an_active_subscription(self):
        m = rc.map_row(row(), "com.example.ios", KINDS, NOW)
        self.assertEqual(m["id"], "apple:2000001")
        self.assertEqual((m["kind"], m["status"], m["reason"]), ("subscription", "completed", "purchase"))
        self.assertEqual((m["price_micros"], m["currency"], m["country"]), (9_990_000, "USD", "US"))
        self.assertEqual(m["expires_at_ms"], rc.micros("2026-12-01 10:00:00") // 1000)
        self.assertEqual((m["was_granted"], m["next_attempt_at_micros"]), (True, 0))

    def test_renewals_trials_refunds_and_ended_periods(self):
        renewal = rc.map_row(row(store_transaction_id="2000002", renewal_number="2"), "a", KINDS, NOW)
        self.assertEqual((renewal["reason"], renewal["original_transaction_id"]), ("renewal", "2000001"))
        trial = rc.map_row(row(is_trial_period="true"), "a", KINDS, NOW)
        self.assertEqual((trial["trial"], trial["price_micros"]), (True, 0))
        refunded = rc.map_row(row(refunded_at="2026-10-05 00:00:00", price_in_purchased_currency="0"), "a", KINDS, NOW)
        self.assertEqual((refunded["status"], refunded["refunded_micros"]), ("revoked", 0))
        ended = rc.map_row(row(end_time="2026-11-01 10:00:00"), "a", KINDS, NOW)
        self.assertEqual(ended["status"], "expired")
        partial = rc.map_row(row(price_in_purchased_currency="4.99"), "a", KINDS, NOW)
        self.assertEqual((partial["status"], partial["refunded_micros"]), ("completed", 5_000_000))
        shared = rc.map_row(row(ownership_type="FAMILY_SHARED"), "a", KINDS, NOW)
        self.assertEqual((shared["family_shared"], shared["price_micros"]), (True, 0))

    def test_one_time_products_take_their_kind_from_the_build(self):
        m = rc.map_row(row(product_identifier="no_ads", is_auto_renewable="false", end_time=""), "a", KINDS, NOW)
        self.assertEqual((m["kind"], m["expires_at_ms"], m["status"]), ("non_consumable", 0, "completed"))
        self.assertIsNone(rc.map_row(row(product_identifier="other"), "a", KINDS, NOW))

    def test_stores_and_stripe_renewal_ids(self):
        g = rc.map_row(row(store="play_store", store_transaction_id="GPA.1-2-3-4"), "a", KINDS, NOW)
        self.assertEqual((g["id"], g["purchase_token"]), ("google:GPA.1-2-3-4", ""))
        s = rc.map_row(row(store="stripe", store_transaction_id="sub_1", original_store_transaction_id="sub_1", renewal_number="3"), "a", KINDS, NOW)
        self.assertEqual((s["id"], s["reason"]), ("stripe:sub_1.3", "renewal"))

    def test_the_latest_version_of_a_transaction_wins(self):
        old = row(updated_at="2026-10-01 10:00:05")
        new = row(updated_at="2026-10-06 00:00:00", refunded_at="2026-10-05 00:00:00")
        kept = rc.latest_versions([new, old])
        self.assertEqual(len(kept), 1)
        self.assertEqual(kept[0]["refunded_at"], "2026-10-05 00:00:00")

    def test_gzipped_csv_and_times(self):
        buf = io.StringIO()
        w = csv.DictWriter(buf, fieldnames=HEADER)
        w.writeheader()
        w.writerow(row())
        path = os.path.join(tempfile.mkdtemp(), "transactions.csv.gz")
        with open(path, "wb") as f:
            f.write(gzip.compress(buf.getvalue().encode()))
        rows = list(rc.open_csv(path))
        self.assertEqual(rows[0]["store_transaction_id"], "2000001")
        self.assertEqual(rc.micros("2026-10-01T10:00:00Z"), rc.micros("2026-10-01 10:00:00"))
        self.assertEqual(rc.micros(""), 0)


if __name__ == "__main__":
    unittest.main()
