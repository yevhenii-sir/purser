# Migrating from RevenueCat

Purser verifies purchases with the stores itself, so the switch is a client change plus, if you want players' history on
day one, an import of RevenueCat's transactions export.

## 1. Set up the app in Purser

Create the app and a build per store (Google Play, App Store, Stripe) on the admin page, with the same product ids as
in RevenueCat ([getting-started.md](getting-started.md), [store-setup.md](store-setup.md)). Point the store
notifications (Google RTDN, App Store Server Notifications V2, the Stripe webhook) at Purser only when the clients
that verify with Purser are out: until then RevenueCat keeps receiving them.

## 2. Import the history (optional)

Without it, a player's purchases appear in Purser when their client restores purchases (the Unity package does it on
first start with a store that reports owned purchases). With it, `/entitlements` knows subscriptions and
non-consumables right away, revenue reports include the past, and the admin page shows the players' history.

1. In RevenueCat, set up **Scheduled Data Exports** (Integrations → Scheduled data exports) with the **transactions**
   feed as CSV, and select at least these columns: `store`, `store_transaction_id`, `original_store_transaction_id`,
   `product_identifier`, `rc_original_app_user_id`, `start_time`, `end_time`, `is_auto_renewable`, `is_trial_period`,
   `is_sandbox`, `refunded_at`, `purchased_currency`, `renewal_number`. Also useful: `updated_at` (picks the latest
   version of a transaction), `effective_end_time`, `purchase_price_in_purchased_currency`,
   `price_in_purchased_currency` (partial refunds), `country`, `ownership_type` (Family Sharing),
   `rc_last_seen_app_user_id_alias`.
2. Download the delivered `transactions_<timestamp>.csv.gz` (a full export, not only the changed rows).
3. Check the mapping without importing, then import (as the database owner, from `server/`):

   ```bash
   python import_revenuecat.py --database <your-billing-db> --csv transactions.csv.gz \
       --app apple=com.example.mygame --app google=com.example.mygame --dry-run
   python import_revenuecat.py --database <your-billing-db> --csv transactions.csv.gz \
       --app apple=com.example.mygame --app google=com.example.mygame
   ```

   `--app <store>=<app_id>` names the build of each store's rows (`apple`, `google`, `stripe`, `manual` for
   RevenueCat's promotional grants); rows of other stores are counted and skipped. Every product of the export must
   exist in the build, or pass `--skip-unknown`. `--user-column rc_last_seen_app_user_id_alias` takes the player id
   from that column when your game identifies players by the alias.

What the import does:

- one ledger row per transaction (`renewal_number` > 1 is a renewal); refunded rows are `revoked`, subscription
  periods that ended are `expired`, the rest `completed`;
- nothing is delivered to the game again and no integration event is sent: the history only;
- trials and Family Sharing rows count as no revenue; partial refunds keep the purchase and count the refunded part;
- the revenue aggregates are rebuilt by the worker after the import;
- repeating the import is safe: rows already in the ledger are skipped.

Limits: the export has no Google purchase token, so Google Play notifications about an imported subscription find it
only after the player's client verifies or restores it once (the Unity package restores on start). Stripe rows get
their ids from RevenueCat (`<id>.<renewal_number>` for renewals), not Stripe's invoice ids.

## 3. Switch the clients

Replace the RevenueCat SDK with the Purser Unity package (or your own client of the HTTP API, [api.md](api.md)) and use
the same player ids (`appUserID` → Purser `user_id`). RevenueCat entitlements map to access levels: give each product
the access levels its RevenueCat entitlements had (Products tab, `access`), and check `HasAccess("premium")` instead
of `customerInfo.entitlements["premium"].isActive`.
