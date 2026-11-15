# Migrating from Adapty or another billing server

## While purchases still go through Adapty

Adapty acknowledges Android purchases itself. The server treats "already consumed/acknowledged" as success, so verifying the same purchases here during the transition is safe.

If Adapty or the client also **consumes** consumables, enable **Client consumes purchases itself** (`client_consumes`) on the app: otherwise such purchases are rejected with `[ERR_CONSUMED_ELSEWHERE]`, because a consumed purchase unknown to the ledger looks like a replay (see "Protection against double grants" in [server/README.md](../server/README.md#protection-against-double-grants)). Turn it off once the client stops consuming.

## Moving the game to Purser's delivery

Purser calls one grant and one revoke reducer of the game's database, each taking a `BillingEvent` ([game-integration.md](game-integration.md)). A game that received purchases through Adapty-era reducers adds these two and keeps its old ones until Adapty is off.

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
