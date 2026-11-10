# Security

## Reporting a vulnerability

Please report security issues privately through GitHub's **Security → Report a vulnerability** on this repository, not in public issues. You will get an answer within a few days.

## How secrets are handled

- Store keys, the billing token and webhook keys are stored in the module's private tables (`credential`). Only the database owner can read private tables; the admin page only writes secrets and lists their names.
- Admin reducers and procedures check the caller against the `admin` table. The database owner cannot be removed, and only the owner can import a ledger or extend the delivery host allowlist.
- The game's `spacetime_token` is sent only over `https` to hosts on the allowlist (`maincloud.spacetimedb.com` and hosts the owner added).
- Store notifications are never trusted: the server re-fetches every purchase from Google or Apple. Google RTDN must carry the shared secret; Apple notifications can be protected with one.
- The setup check never calls the game's reducers and never prints secret values.

## Scope notes

`/verify`, `/restore` and `/entitlements` are public by design: a purchase can only be granted with a token the store confirms. Consumables are bound to the buyer's account id; owned products follow the store account (restore). `/entitlements` reveals what a given `user_id` owns, so don't use guessable user ids if that matters for your game.
