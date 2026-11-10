# Migrating from Adapty or another billing server

## While purchases still go through Adapty

Adapty acknowledges Android purchases itself. The server treats "already consumed/acknowledged" as success, so verifying the same purchases here during the transition is safe.

If Adapty or the client also **consumes** consumables, enable **Client consumes purchases itself** (`client_consumes`) on the app: otherwise such purchases are rejected with `[ERR_CONSUMED_ELSEWHERE]`, because a consumed purchase unknown to the ledger looks like a replay (see "Protection against double grants" in [server/README.md](../server/README.md#protection-against-double-grants)). Turn it off once the client stops consuming.

## Keeping the game's existing reducers

Games that already receive purchases through `server_fulfill_purchase` / `adapty_webhook_event` reducers can keep them with `args_format = legacy` (the app's Delivery tab → Arguments format, or *Advanced* in the Add an app wizard):

- grant → `server_fulfill_purchase(user, tx, product, expires_ms)`;
- subscription revoke → `adapty_webhook_event(user, "subscription_expired", tx, product, expires_ms)`.

Consumable refunds and non-consumable transfers cannot be expressed in this contract: they are only recorded in the ledger. New games should use `v1` ([game-integration.md](game-integration.md)).

## Moving the ledger to another server

The ledger moves together with the database, so the new server knows every granted purchase and `/entitlements` works immediately, without `/restore`:

1. Publish the module on the new server, allow hosts and upload the configuration (`seed.py --server <new>`).
2. Copy the ledger. Import is available only to the database owner (`admin_import_purchases`); existing rows are skipped:
   ```bash
   python migrate.py --from maincloud --from-db <your-billing-db> --to myserver --to-db new-billing --dry-run
   python migrate.py --from maincloud --from-db <your-billing-db> --to myserver --to-db new-billing
   ```
3. Switch clients (`serverBaseUrl`) and store notifications (RTDN, App Store Notifications) to the new address.
4. Run `migrate.py` once more: it picks up purchases the old server recorded after the first copy.

Unfinished rows (`next_attempt_at_micros > 0`) are completed by the new server's worker, but while the old server is still running it may complete them too. That's safe as long as the game is idempotent on `transaction_id`, and for consume “already consumed” already counts as success.
