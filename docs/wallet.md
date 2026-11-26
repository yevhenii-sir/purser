# Virtual currency

A player's balance of each currency of an app (gems, coins), kept by the server: purchases of products that give
currency credit it, refunds take it back, and the game spends it through the server, so a modified client cannot
give itself currency.

## Products that give currency

On the admin page's Products tab, **Virtual currency** of a product: `gems:100`, or several currencies
`gems:100, coins:5000`, per unit bought (a purchase of 3 units gives 300). It is kept per build, like the price
(`product.wallet`, `admin_upsert_products` field `wallet`). Any kind of product may give currency: a subscription
gives it with every paid period (each renewal is a purchase of its own).

- The purchase is granted for the first time: the player gets the currency (once, however often it is verified).
- It is refunded or revoked (a store refund, a revoke from the admin page or `/server/revoke`): the currency is
  taken back. The player may have spent it already, so the balance can go below zero; later purchases pay the
  debt first. A partial refund takes nothing back.
- A refunded purchase is valid again (a dispute won, Apple `REFUND_REVERSED`): the currency is given again.

The game still gets the purchase as a `grant` event (or the client as `GrantOnClient`): with a wallet product it
needs only to refresh the balance it shows.

## Balances and spending

| Who | Route | What |
|---|---|---|
| Client | `POST /wallet` `{"app_id", "user_id"}` | `{"status": "ok", "balances": {"gems": 120}}` (currencies the player never had are missing) |
| Client | `POST /wallet/spend` `{"app_id", "user_id", "currency", "amount", "reference", "note"}` | `{"status": "ok", "balance": 90, "repeated": false}`; only builds with player tokens (`client_auth` = `signed`), else `403 ERR_SIGNED_ONLY` |
| Backend | `POST /server/wallet` `{"app_id", "user_id"}` | balances and the latest 50 changes |
| Backend | `POST /server/wallet/spend`, `/server/wallet/grant` `{"app_id", "user_id", "currency", "amount", "reference", "note"}` | as `/wallet/spend`; `grant` gives currency (rewards) |

The backend routes take a server key with the `wallet` scope (`Authorization: Bearer psk_…`, see
[game integration](game-integration.md)). `reference` is the game's id of the change (1-128 of `A-Z a-z 0-9 _ - . :`,
e.g. `sword-42` or an order id): a request repeated with it changes nothing more and answers `repeated: true`, so a
spend that timed out is retried with the same reference. A reference used for another change is `409
ERR_REFERENCE_USED`; a spend over the balance is `409 ERR_INSUFFICIENT_FUNDS` and spends nothing. Currency ids are
1-32 of `A-Z a-z 0-9 _ -`; amounts and balances are whole numbers up to 10^12.

Unity: `GetWalletAsync()` (`WalletResult.Of("gems")`) and `SpendAsync("gems", 30, "sword-42")`.

## Admin page

The player page shows the balances and the latest changes with their reason (`purchase`, `refund`, `reinstate`,
`spend`, `grant`, `admin`) and reference. **Adjust** (Support role) adds or takes currency with a note
(`admin_wallet_adjust(game_key, user_id, currency, delta, note)`, in the audit log); it cannot take a balance below
zero. A player's export has their wallet; an erasure moves it to the anonymous id like their purchases.

Tables: `wallet_balance` (per app, currency and player) and `wallet_tx` (every change, unique per app and reference).
