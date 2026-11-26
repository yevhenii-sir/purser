# Migrating from Adapty or another billing server

## While purchases still go through Adapty

Adapty acknowledges Android purchases itself. The server treats "already consumed/acknowledged" as success, so verifying the same purchases here during the transition is safe.

If Adapty or the client also **consumes** consumables, enable **Client consumes purchases itself** (`client_consumes`) on the app: otherwise such purchases are rejected with `[ERR_CONSUMED_ELSEWHERE]`, because a consumed purchase unknown to the ledger looks like a replay (see "Protection against double grants" in [server/README.md](../server/README.md#protection-against-double-grants)). Turn it off once the client stops consuming.

## Moving the game to Purser's delivery

Purser calls one grant and one revoke reducer of the game's database, each taking a `BillingEvent` ([game-integration.md](game-integration.md)). A game that received purchases through Adapty-era reducers adds these two and keeps its old ones until Adapty is off.

## Moving the ledger to another server

See [moving to another server](moving-servers.md): the stable address that makes a move invisible to games and stores, everything that knows the server's address, and `migrate.py`.

To bring purchases from another billing system, the owner can call `admin_import_purchases` with ledger rows (at most 500 per call; existing rows are skipped).
