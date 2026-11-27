# Virtual currency

This page covers virtual currency kept by the server. Read it if your game sells or gives currency such as gems or coins.

The server keeps a player's balance of each currency of an app (gems, coins):

- purchases of products that give currency credit it;
- refunds take it back;
- the game spends it through the server, so a modified client cannot give itself currency.

## Products that give currency

1. Open the admin page's **Products** tab.
2. Set the product's **Virtual currency**: `gems:100`, or several currencies `gems:100, coins:5000`.

The amount is per unit bought: a purchase of 3 units gives 300. It is kept per build, like the price
(`product.wallet`, `admin_upsert_products` field `wallet`).

Any kind of product may give currency. A subscription gives it with every paid period, because each renewal is a
purchase of its own.

How the purchase changes the balance:

| The purchase | The currency |
|---|---|
| Is granted for the first time | The player gets it, once, however often the purchase is verified. |
| Is refunded or revoked (a store refund, a revoke from the admin page or `/server/revoke`) | Taken back. The player may have spent it already, so the balance can go below zero. Later purchases pay the debt first. |
| Is partially refunded | Nothing is taken back. |
| Is valid again after a refund (a dispute won, Apple `REFUND_REVERSED`) | Given again. |

The game still gets the purchase as a `grant` event (or the client as `GrantOnClient`). With a wallet product, it
needs only to refresh the balance it shows.

## Balances and spending

| Who | Route | What |
|---|---|---|
| Client | `POST /wallet` `{"app_id", "store", "user_id"}` | `{"status": "ok", "balances": {"gems": 120}}`. Currencies the player never had are missing. |
| Client | `POST /wallet/spend` `{"app_id", "store", "user_id", "currency", "amount", "reference", "note"}` | `{"status": "ok", "balance": 90, "repeated": false}`. Only builds with player tokens (`client_auth` = `signed`), else `403 ERR_SIGNED_ONLY`. |
| Backend | `POST /server/wallet` `{"app_id", "store", "user_id"}` | Balances and the latest 50 changes. |
| Backend | `POST /server/wallet/spend`, `/server/wallet/grant` `{"app_id", "store", "user_id", "currency", "amount", "reference", "note"}` | As `/wallet/spend`. `grant` gives currency (rewards). |

The backend routes take a server key with the `wallet` scope (`Authorization: Bearer psk_…`, see
[game integration](game-integration.md)).

### References

`reference` is the game's id of the change, e.g. `sword-42` or an order id. It is 1-128 of `A-Z a-z 0-9 _ - . :`.

- A request repeated with the same reference changes nothing more and answers `repeated: true`. Retry a spend that
  timed out with the same reference.
- A reference used for another change is `409 ERR_REFERENCE_USED`.
- A spend over the balance is `409 ERR_INSUFFICIENT_FUNDS` and spends nothing.

### Limits

- Currency ids: 1-32 of `A-Z a-z 0-9 _ -`.
- Amounts and balances: whole numbers up to 10^12.

### Unity

Unity: `GetWalletAsync()` (`WalletResult.Of("gems")`) and `SpendAsync("gems", 30, "sword-42")`.

## Admin page

The player page shows the balances and the latest changes, with their reason (`purchase`, `refund`, `reinstate`,
`spend`, `grant`, `admin`) and reference.

**Adjust** (Support role) adds or takes currency with a note. Behind it is
`admin_wallet_adjust(game_key, user_id, currency, delta, note)`, and every adjustment is in the audit log. It cannot
take a balance below zero.

A player's export has their wallet. An erasure moves it to the anonymous id, like their purchases.

Tables:

- `wallet_balance`: per app, currency and player;
- `wallet_tx`: every change, unique per app and reference.
