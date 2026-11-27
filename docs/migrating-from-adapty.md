# Migrating from Adapty or another billing server

This page covers moving a game from Adapty, or another billing server, to Purser. It explains how to run both side by side, how to switch the game to Purser's delivery, and how to bring old purchases along.

## While purchases still go through Adapty

Adapty acknowledges Android purchases itself. The server treats "already consumed/acknowledged" as success. So verifying the same purchases here during the transition is safe.

If Adapty or the client also **consumes** consumables:

1. Enable **Client consumes purchases itself** (`client_consumes`) on the app.
2. Turn it off once the client stops consuming.

Without this setting, such purchases are rejected with `[ERR_CONSUMED_ELSEWHERE]`. A consumed purchase unknown to the ledger looks like a replay (see "Protection against double grants" in [server/README.md](../server/README.md#protection-against-double-grants)).

## Moving the game to Purser's delivery

Purser calls one grant and one revoke reducer of the game's database. Each takes a `BillingEvent` ([game-integration.md](game-integration.md)).

A game that received purchases through Adapty-era reducers adds these two. It keeps its old ones until Adapty is off.

## Moving the ledger to another server

See [moving to another server](moving-servers.md). It covers:

- the stable address that makes a move invisible to games and stores;
- everything that knows the server's address;
- `migrate.py`.

To bring purchases from another billing system, the owner can call `admin_import_purchases` with ledger rows. Each call takes at most 500 rows. Existing rows are skipped.
