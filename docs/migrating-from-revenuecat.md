# Migrating from RevenueCat

This page walks through moving a game from RevenueCat to Purser. Purser verifies purchases with the stores itself, so the switch is a client change. If you want players' history on day one, you also import RevenueCat's transactions export.

## 1. Set up the app in Purser

1. Create the app and a build per store (Google Play, App Store, Stripe) on the admin page. Use the same product ids as in RevenueCat ([getting-started.md](getting-started.md), [store-setup.md](store-setup.md)).
2. Point the store notifications (Google RTDN, App Store Server Notifications V2, the Stripe webhook) at Purser only when the clients that verify with Purser are out. Until then, RevenueCat keeps receiving them.

## 2. Import the history (optional)

- **Without the import**, a player's purchases appear in Purser when their client restores purchases. The Unity package does it on first start with a store that reports owned purchases.
- **With the import**, `/entitlements` knows subscriptions and non-consumables right away, revenue reports include the past, and the admin page shows the players' history.

To import:

1. In RevenueCat, set up **Scheduled Data Exports** (Integrations → Scheduled data exports) with the **transactions** feed as CSV. Select at least the required columns below, and the useful ones if you can.
2. Download the delivered `transactions_<timestamp>.csv.gz`. It is a full export, not only the changed rows.
3. Check the mapping without importing, then import (as the database owner, from `server/`):

   ```bash
   python import_revenuecat.py --database <your-billing-db> --csv transactions.csv.gz \
       --app apple=com.example.mygame --app google=com.example.mygame --dry-run
   python import_revenuecat.py --database <your-billing-db> --csv transactions.csv.gz \
       --app apple=com.example.mygame --app google=com.example.mygame
   ```

Columns of the export:

- **Required**: `store`, `store_transaction_id`, `original_store_transaction_id`, `product_identifier`, `rc_original_app_user_id`, `start_time`, `end_time`, `is_auto_renewable`, `is_trial_period`, `is_sandbox`, `refunded_at`, `purchased_currency`, `renewal_number`.
- **Also useful**: `updated_at` (picks the latest version of a transaction), `effective_end_time`, `purchase_price_in_purchased_currency`, `price_in_purchased_currency` (partial refunds), `country`, `ownership_type` (Family Sharing), `rc_last_seen_app_user_id_alias`.

Options of `import_revenuecat.py`:

- `--app <store>=<id>` names the build of each store's rows. For `apple`, `google` and `stripe` it is the build's id in that store: `apple=com.example.mygame` is the build `apple:com.example.mygame`. RevenueCat's promotional grants (`manual`) go to a build of any store, named by its key: `manual=google:com.example.mygame`. Rows of other stores are counted and skipped.
- Every product of the export must exist in the build, or pass `--skip-unknown`.
- `--user-column rc_last_seen_app_user_id_alias` takes the player id from that column. Use it when your game identifies players by the alias.

What the import does:

- It writes one ledger row per transaction (`renewal_number` > 1 is a renewal). Refunded rows are `revoked`, subscription periods that ended are `expired`, the rest `completed`.
- Nothing is delivered to the game again and no integration event is sent: the history only.
- Trials and Family Sharing rows count as no revenue. Partial refunds keep the purchase and count the refunded part.
- The worker rebuilds the revenue aggregates after the import.
- Repeating the import is safe: rows already in the ledger are skipped.

Limits:

- The export has no Google purchase token. Google Play notifications about an imported subscription find it only after the player's client verifies or restores it once. The Unity package restores on start.
- Stripe rows get their ids from RevenueCat (`<id>.<renewal_number>` for renewals), not Stripe's invoice ids.

## 3. Switch the clients

1. Replace the RevenueCat SDK with the Purser Unity package (or your own client of the HTTP API, [api.md](api.md)).
2. Use the same player ids (`appUserID` → Purser `user_id`).
3. Give each product the access levels its RevenueCat entitlements had (Products tab, `access`). RevenueCat entitlements map to access levels.
4. Check `HasAccess("premium")` instead of `customerInfo.entitlements["premium"].isActive`.
