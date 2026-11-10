# Purser server: in-app purchase billing on SpacetimeDB

A replacement for Adapty: a SpacetimeDB module that verifies Google Play, App Store and Amazon Appstore purchases itself, keeps a transaction ledger, consumes/acknowledges Google purchases, delivers rewards into the game and sends events to GA4. The client engine doesn't matter: GameMaker, Unity and anything else work over plain HTTP+JSON.

This is the server reference: tables, secrets, the HTTP API and admin calls. To get started, see the [quick start](../README.md#quick-start).

- Your database name goes into `spacetime.json` (copy `spacetime.example.json`) for `seed.py`, `migrate.py` and `mcp_server.py`. The scripts use the token of `spacetime login`, or `PURSER_TOKEN` when it is set.
- `mcp_server.py` — an MCP server for Claude and other MCP clients; see [docs/mcp.md](../docs/mcp.md).
- SpacetimeDB **2.14.1**, HTTP handlers (`features = ["unstable"]`, beta).
- `core/` — pure logic without SpacetimeDB (JWT, parsing store responses and notifications, delivery contract), covered by tests.
- `spacetimedb/` — the module: tables, admin reducers, HTTP routes, ledger and worker.

```
client ──POST /verify/<store>──▶ billing ──▶ Google / Apple / Amazon API (verification)
                                         │
                                         ├─▶ purchase ledger (verified)
                                         ├─▶ delivery to the game: SpacetimeDB reducer or webhook  (granted)
                                         ├─▶ Google consume / acknowledge                         (completed)
                                         └─▶ GA4 purchase
Google RTDN / App Store Notifications V2 / Amazon RTN ──▶ /notify/<store> ──▶ renewals, refunds, expirations
```

## Tokens: one, or a new one every time?

| Target | How |
|---|---|
| SpacetimeDB HTTP API (billing → game database) | One long-lived token (`POST /v1/identity` → `{identity, token}`, the token has `exp: null`). Stored in `credential` as `spacetime_token` and sent as `Authorization: Bearer …`. No need to reconnect. |
| Google Play Developer API | A JWT (RS256) is signed with the service account key and exchanged for an OAuth token (valid for 1 h). The token is cached in `access_token_cache` and refreshed when less than 5 min remain. |
| App Store Server API | We sign the JWT ourselves (ES256, `.p8` key), valid for 50 min, cached per account + bundle id. |
| Amazon Receipt Verification Service | The shared key of the developer account is part of the request URL; nothing to refresh. |
| Steam `ISteamMicroTxn` | The publisher Web API key goes in the `x-webapi-key` header of every call (never in a URL); nothing to refresh. |

## SpacetimeDB limitations to keep in mind

- Every outgoing HTTP request from the module has a **timeout of at most 500 ms**. That's why a ledger row is written first and every external step is retryable: whatever didn't finish is completed by the `process_pending` worker (once a minute) with backoff 1m → 5m → 15m → 1h → 3h → 6h → 12h.
- HTTP to private addresses (`127.0.0.1`, `10.x`, …) is forbidden: the webhook and the game database must be publicly reachable.
- HTTP handlers are in beta. If the API breaks, the same logic can be called as procedures through the stable `POST /v1/database/<db>/call/<procedure>`.

## Admin panel

`https://maincloud.spacetimedb.com/v1/database/<your-billing-db>/route/admin` — a page for configuration without the CLI. The module serves it itself (`GET /admin`), so no separate hosting is needed. On a self-hosted server where `/sql` and `/logs` are closed to the outside, open the admin panel through an SSH tunnel ([docs/self-hosting.md](../docs/self-hosting.md)).

Features:
- **Overview:** server status, counters (`admin_overview`), net revenue for 30 days, purchases by status, “stuck” purchases with errors.
- **Revenue:** net revenue, refunds, purchases, subscriptions and MRR in USD, charts and breakdowns by app, product and currency; see [docs/analytics.md](../docs/analytics.md).
- **Apps:** one page per app (its Android and iOS builds share a `game_key`) with the tabs **Setup** (setup check, URLs to paste into the stores), **Products** (one table for both builds, **Import from Google Play**, copy to iOS), **Delivery**, **Integrations**, **Keys** (every secret the builds need and where it comes from), **Purchases** and **Revenue**. The **Add an app** wizard creates an app end to end.
- **Developer accounts:** keys shared by every app of an account. **Server:** allowed SpacetimeDB hosts, the public address, all secrets by scope.
- **Secrets:** list of names and writing new values (a `.json` / `.p8` file can be uploaded). The page never reads or displays values.
- **Integrations:** connecting AppsFlyer, event names, “Send test event”.
- **Purchases:** the newest purchases of a period, filtered by app and status on the server (`admin_list_purchases`); search by player, transaction id or token; purchase history; “Retry now”; manual grant; entitlements check.
- **Player profile** (`#player/<user_id>`, opened from **Open player** on Purchases, “Find player / transaction” in Ctrl+K, or any player id in a table or purchase): one player across every app: builds seen, first and last purchase, spending in USD (same rules as Revenue), entitlements exactly as `/entitlements` returns them, every ledger row, attribution ids, delivery problems. Actions: manual grant, revoke (with a confirmation and a reason), retry now, copy ids. Data comes from the `admin_user_profile` and `admin_find_player` procedures.
- Module **Logs** and **Admins**.
- **Alerts:** Telegram, Slack, Discord or webhook messages about failing deliveries, rejected store keys, dropped integration events, silent or rejected store notifications; see [docs/alerts.md](../docs/alerts.md).

Sign in with the database owner's token:

```bash
spacetime login show --token
```

Only the owner can read private tables, and admin reducers check the identity against the `admin` table. The page itself contains nothing secret. The token is stored only in the browser: in `sessionStorage`, or in `localStorage` if “Remember” is checked. The page is served with CSP (`connect-src 'self'`), `X-Frame-Options: DENY` and `no-store`.

## Configuration

Everything is configured through admin reducers. Admins are the identity that published the database (`init`) plus anyone added via `admin_add_admin`. If the module was published over a database created in the dashboard, `init` did not run: the owner adds itself with `spacetime sql <db> "INSERT INTO admin (identity, protected) VALUES (0x<your identity>, true)"` (only the owner can write private tables) and starts the worker with `spacetime call <db> admin_ensure_worker`. The admin page does both with one button. All tables are private: only the owner can read them via `spacetime sql`.

### Apps (`game_app`)

One row = one **build**: an app in one store. The builds of the same game (Google Play, App Store, Amazon Appstore, ...) share a common `game_key`, so purchases are visible on all of them.

| Field | Meaning |
|---|---|
| `app_id` | unique id of the build: the package / bundle id for Google Play and the App Store (part of the store API calls); any id for the Amazon Appstore, e.g. `com.example.mygame.amazon` when the Google Play build already uses the package name |
| `platform` | `android` \| `ios` \| `amazon` \| `steam` \| `web` |
| `store` | `google` \| `apple` \| `amazon` \| `steam` \| `stripe` \| `xsolla`; empty = the platform's default store (`android` → `google`, `ios` → `apple`, `amazon` → `amazon`, `steam` → `steam`). A `web` build must name its store. Rows written before the column existed have it empty |
| `game_key` | shared game key for `/entitlements` |
| `account` | secrets profile (developer account), see below |
| `delivery` | `spacetimedb` \| `webhook` \| `none` (the game needs no server of its own, the client grants the reward, see “Games without a backend”) |
| `spacetime_url`, `spacetime_db`, `grant_reducer`, `revoke_reducer`, `args_format` | for `spacetimedb`; `args_format` = `legacy` \| `v1` |
| `webhook_url` | for `webhook` (https only) |
| `allow_sandbox` | accept sandbox / license-tester purchases (needed for App Review and QA) |
| `client_consumes` | the client consumes in Google itself (for example, while purchases still go through Adapty). Off (default): a consumable that Google reports as already consumed but that isn't in the ledger is rejected, see “Protection against double grants” |
| `apple_refund_mode` | App Store builds: how refund requests (`CONSUMPTION_REQUEST`) are answered: empty / `off`, `inform`, `prefer`; see [docs/store-setup.md](../docs/store-setup.md#refund-requests). Answers wait in `apple_consumption_outbox` |

### SpacetimeDB hosts for delivery (`spacetime_host`)

The game's `spacetime_token` is sent to `spacetime_url`. That's why delivery only goes over `https://` and only to allowed hosts. `maincloud.spacetimedb.com` is always allowed. Your own servers, e.g. `stdb.example.com`, are added to the `spacetime_host` table: in the admin panel it's Settings → Server → SpacetimeDB hosts, via CLI — `admin_add_spacetime_host` / `admin_remove_spacetime_host`, via `seed.py` — the `spacetime_hosts.json` file.

Only the **database owner** can change the list, regular admins can't: otherwise an admin could redirect delivery, along with the token, to their own server. An app with a disallowed host won't be saved. If a host is removed, delivery to it fails and keeps retrying until the app is moved to another host.

### Stores and platforms

Every per-store fact lives in one registry, [`core/src/registry.rs`](core/src/registry.rs): the platforms a store serves, its secrets (and which are per developer account), its routes, whether purchases come from client receipts (`/verify/<store>`) or webhooks (`/notify/<store>` only), what happens after the grant (Google consume / acknowledge, a client-side finish, nothing), whether the store reports prices, account binding, catalog import and subscription re-checks. The module's routes, verification, setup check and secret names and the admin page (`admin-ui/src/lib/registry.json`, a snapshot kept in sync by a test) read it.

| Store | Platform | Verification | Notifications | Secrets |
|---|---|---|---|---|
| `google` | `android` | `/verify/google` (purchase token) | `/notify/google?secret=` (RTDN) | `google_service_account_json`, `google_rtdn_secret` |
| `apple` | `ios` | `/verify/apple` (transaction id) | `/notify/apple` (ASSN V2) | `apple_issuer_id`, `apple_key_id`, `apple_private_key_p8`, `apple_notification_secret` |
| `amazon` | `amazon` | `/verify/amazon` (receipt id + Amazon user id) | `/notify/amazon?app=<app_id>` (RTN via SNS) | `amazon_shared_secret` |
| `xsolla` | `web` | none: checkout `/checkout/xsolla`, granted by webhooks | `/notify/xsolla?app=<app_id>` (signed webhooks) | `xsolla_project_id`, `xsolla_api_key`, `xsolla_webhook_secret` |
| `stripe` | `web` | webhooks; `/checkout/stripe` creates the Checkout Session, `/verify/stripe` confirms one after the redirect | `/notify/stripe?app=<app_id>` (signed webhooks) | `stripe_secret_key`, `stripe_webhook_secret` |
| `steam` | `steam` | `/checkout/steam` (server `InitTxn`), then `/verify/steam` (order id) | none: the worker polls `GetReport` | `steam_publisher_key` |

### Products (`product`)

`kind`: `consumable` (Google consume), `non_consumable` (acknowledge), `subscription` (acknowledge). Amazon `CONSUMABLE` / `ENTITLED` / `SUBSCRIPTION` receipts must match the kind. `default_price_micros` + `currency` are used for GA4 revenue when neither the store nor the client sent a price.

### Secrets (`credential`) — bound to an app or an account

A secret key is `{scope}/{name}`, where scope is an app id, an account name or `default`. Lookup follows the chain **`{app_id}` → `{account}` → `default`**. This way apps from different Google Play / App Store Connect accounts and games on different SpacetimeDB servers each use their own keys. The token cache is also split by the scope the key came from.

| `name` | What it is |
|---|---|
| `google_service_account_json` | JSON key of a service account with access to the app in Play Console (Financial data + Manage orders) |
| `google_rtdn_secret` | secret in the RTDN push subscription URL |
| `apple_issuer_id`, `apple_key_id`, `apple_private_key_p8` | App Store Connect → Users and Access → Integrations → In-App Purchase |
| `apple_notification_secret` | optional secret in the Apple notifications URL |
| `amazon_shared_secret` | Amazon Developer Console → Shared Key (one per developer account) |
| `xsolla_project_id`, `xsolla_api_key`, `xsolla_webhook_secret` | Xsolla Publisher Account: the project ID (not secret, kept with the keys), an API key (company-wide or per project), the project's webhook secret key |
| `stripe_secret_key` | Stripe secret key `sk_live_…` / `sk_test_…` or a restricted key `rk_…` (one per Stripe account) |
| `stripe_webhook_secret` | signing secret `whsec_…` of the build's webhook endpoint |
| `steam_publisher_key` | Steamworks publisher Web API key (Users & Permissions → Manage Groups; one per publisher group) |
| `spacetime_token` | long-lived token of an identity trusted by the game module |
| `webhook_secret` | HMAC key for webhook delivery |
| `alert_telegram_bot_token`, `alert_slack_webhook_url`, `alert_discord_webhook_url`, `alert_webhook_secret` | alert channels, always in the `default` scope; see [docs/alerts.md](../docs/alerts.md) |

### Loading: `seed.py`

```bash
cd server
cp -r seed.example secrets
python seed.py
```

In `secrets/` (the folder is in `.gitignore`) fill in `spacetime_hosts.json`, `apps.json`, `products.json`, `credentials.json`; key files are referenced in the `value_file` field. The script takes the server and database from `spacetime.json` (`--server` / `--database` override them) and calls `admin_add_spacetime_host`, `admin_upsert_game_app`, `admin_upsert_products`, `admin_set_credential`. Hosts are loaded first, because apps are validated against that list. Placeholders like `<…>` are skipped.

After `integrations.json` it loads, when present: `paywalls.json` (`admin_upsert_paywall`), `placements.json` (`admin_upsert_placement`; A/B tests are started in the admin page), `alerts.json` (`{"channels": [{kind, enabled, settings, secret?}], "rules": {...}}` → `admin_upsert_alert_channel`, `admin_set_alert_rules`; a channel without `secret` keeps the stored one) and `settings.json` (`public_base_url`, `store_commission_percent`, `store_commission_by_app`, `store_commission_after_year_percent` → `admin_set_setting`). The format of every file is in the docstring of `seed.py` and in `seed.example/`.

- `python seed.py --export DIR` writes the configuration of the database to `DIR` in the same format (owner only; it refuses a non-empty `DIR` without `--force`). Secret values are never read: `credentials.json` lists the keys that are set with the value `<set me>`, which `seed.py` skips until you fill it in.
- `python seed.py --init` is a guided first setup: it asks for the database, the app and its Google Play / App Store / Amazon builds, the delivery and the store keys (the questions come from the store registry), generates the notification secrets, writes the answers to `--dir` (default `secrets/`), uploads them, offers the Google Play catalog import, runs the setup check and prints the notification URLs and the Unity `BillingConfig` JSON.

### Setup check

`admin_check_app(app_id, options_json)` (a procedure; the app's **Setup** tab in the admin panel, `python seed.py --check-only`) checks one app and returns a report with a level (`ok` / `warn` / `fail`) and a fix for every item:

- the app and its products (Android products without a default price are counted as 0 revenue);
- which secrets are set and from which scope; the service account's `client_email` (the account to invite in Play Console), the `.p8` key;
- Google Play API access with the app's permissions; App Store Server API access and, on the next run, whether Apple delivered the test notification to the server; whether Amazon RVS accepts the shared secret; whether Xsolla accepts the API key and lists the products as enabled virtual items; whether Stripe accepts the key (and its mode fits the build's sandbox setting) and every product is an active Stripe Price of the right type;
- whether store notifications arrive, and whether they arrive with a wrong secret or signature; for Amazon the endpoint confirmation token to paste into the Developer Console;
- delivery: the host allowlist, the billing identity and its token, and that the game database has the grant / revoke reducers with the right number of arguments (they are never called); webhook reachability;
- GA4 (validation endpoint), AppsFlyer settings, purchases that keep failing.

`options_json`: `{"online": false}` checks only the configuration (no HTTP); `{"apple_test": false}` skips the Apple test notification. `admin_ping_webhook(app_id)` sends a signed `{"event":"ping"}` to the webhook. `admin_list_store_products(app_id)` lists the store catalog compared with the configured products: Google Play (one-time products and subscriptions with US prices) or the App Store (in-app purchases and subscriptions through the App Store Connect API, without prices; needs the Team key secrets `apple_connect_key_id` and `apple_connect_private_key_p8`, see [docs/store-setup.md](../docs/store-setup.md#catalog-import-optional)).

Other admin reducers: `admin_set_setting("public_base_url", url)` (also `store_commission_percent`, `store_commission_after_year_percent`, `store_commission_by_app` for the proceeds estimate, see [docs/analytics.md](../docs/analytics.md)), `admin_delete_game_app`, `admin_delete_product`, `admin_delete_credential`, `admin_add_admin`, `admin_remove_admin`, `admin_requeue_purchase(id)` (retry now), `admin_grant_manual(json)` (manual grant: compensations and promos; goes through the same ledger and delivery, not sent to GA4), `admin_revoke_purchase(purchase_id, reason)` (procedure: revokes a purchase the player owns through the ledger; the game receives `revoke` with `reason = admin`; idempotent).

**An admin revoke is not a refund.** Revenue and integrations stay as they are, because the store kept the money; the purchase is marked in the `admin_revoke` table. If the store refunds it later, the refund is counted then (and sent to integrations) without a second `revoke` to the game. A store purchase that the store still reports as active comes back when the player restores it: the store stays the source of truth for paid products.

## HTTP API

Base: `https://maincloud.spacetimedb.com/v1/database/<your-billing-db>/route`

### `POST /verify/<store>`: `/verify/google`, `/verify/apple`, `/verify/amazon`, `/verify/steam`

One route per client-receipt store of the registry; a build's purchases go to its store's route. The body:

```json
{
  "bundle_id": "com.example.mygame",
  "user_id": "<player identity hex>",
  "product_id": "diamonds_150",
  "purchase_token": "<Google purchaseToken>",
  "order_id": "<Apple transactionId / Amazon receiptId (required) / Google orderId (optional)>",
  "store_user_id": "<Amazon userId (required for Amazon)>",
  "app_instance_id": "<Firebase app_instance_id, optional>",
  "price_micros": 2990000,
  "currency": "USD"
}
```

| HTTP | `status` | What the client should do |
|---|---|---|
| 200 | `success` | Reward granted. The transaction can be finished in the SDK. |
| 202 | `pending` | Purchase verified and recorded, the worker will deliver the reward. The transaction can be finished. |
| 200 | `pending_or_canceled` | Payment hasn't gone through yet (or was canceled). Retry later. |
| 200 | `expired` / `revoked` | Subscription expired / purchase refunded. |
| 400 | `invalid` / `error` | The store doesn't know this purchase, or the request is invalid. |
| 403 | `forbidden` | The consumable already belongs to another player or is bound to another account; the consumable was already consumed outside this server (`[ERR_CONSUMED_ELSEWHERE]`); sandbox is not allowed. |
| 502 / 500 | `error` | Store unavailable / configuration error. Retry later. |

The response also contains `transaction_id`, `product_id`, `is_sub`, `expires_date_ms`, `environment`, `token`.

`/verify/steam` takes `{"app_id": "<Steam App ID>", "user_id": "...", "order_id": "<order_id from /checkout/steam>"}` (`product_id` optional: the order knows it) after the game received `MicroTxnAuthorizationResponse_t`. The server calls `FinalizeTxn`, then `QueryTxn` (or `GetUserAgreementInfo` for a subscription), and records `steam:<transid>`. Only the player who started the checkout can verify its order. A denied authorization answers `pending_or_canceled`. Calling it again is safe: a finalized order is only queried.

### `POST /checkout/steam`

`{"app_id": "480", "product_id": "gems_100", "user_id": "...", "steam_id": "<SteamID64>", "language": "<ISteamApps::GetCurrentGameLanguage(), e.g. english>"}`. The server asks `GetUserInfo` for the player's wallet currency and country, picks the price (the item's price in that currency, else the catalog `default_price_micros` + `currency`, which Steam converts), generates the order id and calls `InitTxn`; Steam shows the overlay. Answer: `{"status": "ok", "order_id": "<u64 as a string>", "trans_id": "...", "currency": "EUR", "amount": 179, "environment": "production" | "sandbox"}` (`amount` in hundredths). Errors: 400 `error` (bad request, e.g. not logged in to Steam), 403 `forbidden` (locked account, restricted country), 500 / 502 `error` (configuration / Steam unavailable).

Orders live in `steam_order` (`initiated` → `authorized` → `finalized`, or `denied` / `failed` / `refunded`). The worker re-queries orders the client never verified (10 min after the checkout, backoff to 24 h, then given up), finalizes ones the player authorized, and every 5 minutes per build polls `GetReport` (`GAMESALES`, cursor in `steam_report`) to revoke refunded and charged-back purchases. Steam settings per build: `admin_set_steam_build` (`{"app_id", "steam_app_id"?, "sandbox"?}`) and per product `admin_upsert_steam_items` (`{"app_id", "product_id", "item_id"?, "description"?, "prices"?: {"USD": 199}, "period"?, "frequency"?}`), or `steam.json` for `seed.py`. See [docs/store-setup.md](../docs/store-setup.md#steam).

### `POST /entitlements`

`{"app_id": "...", "user_id": "..."}` → active subscriptions and non-consumables across all apps with the same `game_key`:

```json
{"subscriptions": {"month_sub": 1793876523123}, "non_consumables": ["one_time_sub"], "server_time_ms": 1791188212478, "game_key": "...", "user_id": "..."}
```

Replaces `adapty_is_purchased` / `adapty_get_subscription_expires_at_ms`.

### `POST /restore`

`{"app_id": "...", "user_id": "...", "store_user_id": "<Amazon userId>", "purchases": [{"product_id": "...", "purchase_token": "..." | "order_id": "..."}]}` (up to 20; an item may carry its own `store_user_id`). Runs verify for every purchase the store SDK returned on “Restore purchases” and returns `{results, entitlements}`.

### `POST /offers/apple/sign`

`{"bundle_id": "...", "product_id": "...", "offer_id": "...", "user_id": "...", "transaction_id": "<optional>"}` → a signature for an App Store **promotional offer** of a subscription: `{status, key_id, nonce, timestamp, signature, app_account_token, signed_offer_jws, expires_at_ms}`. The signature is bound to the player's `appAccountToken` and made with the build's In-App Purchase key (`apple_key_id`, `apple_private_key_p8`; `apple_issuer_id` for the V2 `signed_offer_jws`). `400` for an unknown/disabled/Android build or a product that is not a subscription, `500 [ERR_OFFER_KEY]` when the key is missing. See [docs/apple-offers.md](../docs/apple-offers.md).

### `POST /paywall`

`{"app_id": "...", "placement": "main_paywall", "user_id": "..."}` → what to show at a placement:
`{placement, paywall_id, paywall_name, variant_id, test_id, products: [{id, store_id, kind}], remote_config}`.
During an A/B test the player is assigned by a hash of the test id and `user_id` (no `user_id`: the
default variant). Unknown or disabled placement: `404 {"status": "not_found", "code": "ERR_PLACEMENT_NOT_FOUND"}`.
`/verify` accepts optional `placement`, `variant_id`, `test_id`, `paywall_id` to attribute the purchase.
See [docs/paywalls.md](../docs/paywalls.md).

### `POST /paywall/event`

`{"app_id": "...", "placement": "...", "variant_id": "...", "test_id": "...", "event": "view"}` → `200 {"status": "ok"}`.
A paywall view for A/B test results (counted per day and variant).

### `POST /notify/google?secret=<google_rtdn_secret>`

Play Console → Monetization setup → Real-time developer notifications → Pub/Sub topic. Create a **Push** subscription to this URL. A test notification from the console should return `200 {"status":"ok","message":"test"}`. The module doesn't trust notification contents: it always re-queries Google by `purchaseToken`. The exception is voided purchases, which are protected by the secret.

### `POST /notify/apple`

App Store Connect → App Information → App Store Server Notifications → **Version 2**, Production and Sandbox URL. You can add `?secret=` and set `apple_notification_secret`. Only `transactionId` is taken from `signedPayload`; the transaction is re-queried from Apple. You can test the setup with `POST /inApps/v1/notifications/test`. A `CONSUMPTION_REQUEST` (a player asked for a refund) is answered with `PUT /inApps/v2/transactions/consumption/{transactionId}` when the build's `apple_refund_mode` is on and the player consented (`attribution.refund_data_consent = "true"`).

### `POST /notify/amazon?app=<app_id>`

Amazon Developer Console → the app → **App Services → Real-Time Notifications** → endpoint URL (one per app, so `?app=` names the build). Amazon delivers through Amazon SNS: the module verifies the SNS signature (certificate from `https://sns.<region>.amazonaws.com/…pem`, cached for a day, SHA1 and SHA256 signatures) and rejects anything else with 401. The endpoint confirmation message is stored, and the next setup check shows its token: paste it into the console within 48 hours. Notifications only name a receipt: the module looks it up again with RVS and applies refunds (`*_CANCELLED`), renewals and expirations. Receipts the client has not verified yet are ignored.

Without notifications, Amazon subscriptions are still re-checked by the worker shortly after their renewal date (`store_poll` table; backoff 1 h → 24 h, then dropped until the client verifies or restores again). An Amazon subscription keeps one receipt across renewals: a renewal updates `expires_at_ms` of its ledger row and the game gets `grant` again with the new expiry (no separate renewal revenue).

### `POST /checkout/xsolla`

`{"app_id": "<Xsolla build>", "product_id": "<item SKU>", "user_id": "...", "sandbox": false, "return_url": "https://…", "country": "DE", "currency": "EUR", "language": "de", "attribution": {…}}` (all but the first three optional) → `200 {"status": "ok", "token", "order_id", "url", "sandbox", "product_id"}`. Creates an Xsolla Pay Station payment token for one item (`POST https://store.xsolla.com/api/v3/project/<project_id>/admin/payment/token`, Basic `project_id:api_key`, one call) with the billing `user_id` as the Xsolla user id; open `url` (`https://secure.xsolla.com/paystation4/?token=…`, sandbox: `sandbox-secure.xsolla.com`). Without `country` the server forwards the first `X-Forwarded-For` address as `X-User-Ip`. `400 error` for an unknown build / product, a subscription product or a bad value; `400 invalid` when Xsolla refuses the item (unknown or disabled SKU); `403 forbidden` for `sandbox` on a build without sandbox purchases; `500` for missing or rejected keys (an owner alert); `502` when Xsolla is unavailable.

### `POST /checkout/xsolla/status`

`{"app_id": "...", "user_id": "...", "order_id": "<from /checkout/xsolla>"}` → the order's state from the ledger, in the shape of `/verify`: `200 success` (granted), `202 pending` (paid, delivery to the game pending), `200 pending_or_canceled` (no payment webhook yet), `200 revoked` / `expired`, `403 forbidden` (another user's order); with `transaction_id`, `product_id`, `is_sub`, `expires_date_ms`, `environment`, `delivery`. No Xsolla call: poll it every few seconds while the player pays.

### `POST /notify/xsolla?app=<app_id>`

Publisher Account → the project → **Project settings → Webhooks**: this URL, and the project's secret key stored as `xsolla_webhook_secret`. Every webhook must carry `Authorization: Signature <sha1(raw body + secret key)>`; a wrong or missing signature answers `400 {"error":{"code":"INVALID_SIGNATURE"}}` (shown by the setup check), a webhook of another project (`settings.project_id` ≠ `xsolla_project_id`) `400 INVALID_PARAMETER`. Success is `204` without a body.

- `user_validation`: `204` for any well-formed billing user id (users are not registered on this server), else `400 INVALID_USER`.
- `order_paid` (combined model, payment data under `billing`) and `payment` (separate model): recorded for the user id of the payment token and granted; the ledger id is `xsolla:<transaction id>`, so both webhooks of one payment and Xsolla's retries grant once. The product is the first bought item SKU that is in the catalog (bundle contents and bonus items are skipped), or the subscription plan id. Price and currency: `payment_details.payment` (what the player paid), else the purchase total; the country from `user.country`; `transaction.dry_run = 1` (or a sandbox order) is a sandbox purchase, ignored unless the build allows sandbox purchases. An `order_paid` without payment data (separate model) is acknowledged and left to the `payment` webhook. An unknown SKU answers `500`, so Xsolla retries after the product is added.
- `refund`, `order_canceled`: the purchase is revoked (`revoke` with reason `refund`, counted as a refund in revenue). `partial_refund` is acknowledged and changes nothing (the order stays paid).
- Subscriptions (best effort; sold outside `/checkout/xsolla`): a `payment` with `purchase.subscription` grants the plan id product until `date_next_charge` (renewals are new transactions in the same chain, `purchase_token = subscription:<id>`); `cancel_subscription` shortens the subscription to `date_end`, or expires it (revoke) when `date_end` has passed. `create_subscription`, `update_subscription`, `non_renewal_subscription` and other types are acknowledged with `204`.

### `POST /checkout/stripe`

`{"app_id": "<Stripe build>", "product_id": "<Stripe Price id>", "user_id": "...", "success_url": "https://…", "cancel_url": "https://…"}` → `{"status": "ok", "session_id": "cs_…", "url": "https://checkout.stripe.com/…"}`. Creates a Checkout Session (one call to Stripe): `mode=payment` for consumables and non-consumables, `mode=subscription` for subscriptions, the user in `client_reference_id`, `app_id` / `product_id` / `user_id` in the metadata of the session and of its PaymentIntent or Subscription. The return URLs must be https (`http://localhost` too on builds with sandbox purchases); `success_url` may contain `{CHECKOUT_SESSION_ID}`. Open `url` in the browser.

### `POST /verify/stripe`

`{"app_id": "...", "user_id": "...", "session_id": "cs_…"}` (also accepts the `/verify/<store>` body with `bundle_id` and `order_id`). Optional: lets the client get its answer right after the redirect instead of waiting for the webhook. Fetches the session (with its Subscription) and records it like the webhook would; the answer has the shape of `/verify/<store>`: `success` / `pending` once paid, `pending_or_canceled` while the session is open or after it expired, `403 forbidden` for a session of another user. Either path grants once.

### `POST /notify/stripe?app=<app_id>`

Stripe webhooks of one build (an endpoint per build, its signing secret in `stripe_webhook_secret`). `Stripe-Signature` is checked (HMAC-SHA256 of `{t}.{body}`, any `v1`, 5 minutes tolerance); a failure answers 400 and shows in the setup check. Events:

| Event | Effect |
|---|---|
| `checkout.session.completed`, `checkout.session.async_payment_succeeded` | grants a paid one-time purchase (transaction id = PaymentIntent id) or the first subscription period (transaction id = Subscription id; one call fetches the Subscription for the period end) |
| `invoice.paid`, `invoice.payment_succeeded` | `subscription_create`: the first period (same row); `subscription_cycle`: a renewal (transaction id = Invoice id, original = Subscription id) |
| `customer.subscription.updated` (to `canceled` / `unpaid` / `incomplete_expired`), `customer.subscription.deleted` | the newest period expires (`revoke` with `expire`) |
| `charge.refunded` (full refunds only), `charge.dispute.created` | revokes the purchase or period and counts a refund; a subscription payment is found by its invoice (`charge.invoice`, or one `GET /v1/invoice_payments` call on newer API versions) |

Prices come from the event (minor units, zero- and three-decimal currencies handled); test-mode events (`livemode: false`) are sandbox purchases; the country is the customer's billing address. Retried and duplicate events are idempotent (the ledger id is the store transaction), and an event never makes a refunded or ended purchase active again. Events without our metadata (Payment Links, other integrations) are acknowledged and ignored.

### `GET /healthz`

## Delivery to the game

The event format, the reducer / webhook contract, idempotency rules, transfers and games without a backend are in [docs/game-integration.md](../docs/game-integration.md).

## Integrations (GA4, AppsFlyer, Adjust, Tenjin, AppMetrica, webhook, Cloud Storage)

Every significant purchase event — `purchase`, `subscription_start`, `renewal`, `refund`, `expire` — is queued in `integration_outbox` in the same transaction as the status change. The `send_integration_event` procedure sends the event to every integration enabled for the app. On network errors, 429 and 5xx it retries (up to 6 attempts, backoff 1m → 5m → 15m → 1h → 3h). A 4xx response is final: it is logged and raises the `integration_failed` alert.

`integration_health` keeps, per build and integration, when an event was last sent, the last failure (retried or dropped) and the last event skipped because the client did not send the device id the integration needs (`appsflyer_id`, `adjust_id`, ...). The Integrations tab shows it under each build ("Sent 5 min ago", "Failed, retrying", "Event dropped", "Skipped: no device id") and the setup check reports it: a failure or a skip after the last sent event is a warning (a dropped event is a failure).

Integrations are configured per build on the app's **Integrations** tab in the admin panel, or with `admin_upsert_integration` (`{"app_id": "...", "kind": "...", "enabled": true, "settings": {...}}`) plus the integration's secret (`admin_set_credential`, resolved app → account → default like every secret):

| `kind` | Settings | Secret | The client sends in `attribution` |
|---|---|---|---|
| `ga4` (Measurement Protocol) | `firebase_app_id`, `send_sandbox` | `ga4_api_secret` | optionally `app_instance_id` (a field of `/verify`) |
| `appsflyer` (S2S Events API v3) | `app_id` (package or `id123456789`), `send_sandbox`, `event_names` | `appsflyer_s2s_key` | `appsflyer_id`; recommended `advertising_id` / `idfa`, `idfv`, `att` |
| `tenjin` (S2S Purchase API) | `send_sandbox`, `send_expire` | `tenjin_sdk_key` | `tenjin_id` (analytics_installation_id), `os_version`; recommended `advertising_id` / `idfa`, `idfv`, `app_version` |
| `adjust` (S2S events) | `app_token`, `event_tokens` (kind → token), `send_sandbox` | `adjust_s2s_token` | `adjust_id` (Adjust.getAdid) or `advertising_id` / `idfa` / `idfv` |
| `appmetrica` (Post API) | `application_id`, `use_profile_id`, `send_sandbox` | `appmetrica_post_api_key` | `appmetrica_device_id` (or the profile id = billing `user_id`); recommended `advertising_id` / `idfa`, `idfv`, `os_version`, `app_version` |
| `webhook` (signed JSON) | `url` (https), `send_sandbox`, `events` (empty: all) | `integration_webhook_secret` | — |
| `gcs` (Cloud Storage export) | `bucket`, `folder`, `send_sandbox`, `events` (empty: all) | `gcs_service_account_json` | — |

Sandbox / license-tester purchases are not sent unless `send_sandbox` is on.

The client passes device identifiers (`appsflyer_id`, `tenjin_id`, `adjust_id`, `appmetrica_device_id`, `advertising_id`, `oaid`, `idfa`, `idfv`, `att`, `os_version`, `app_version`, and the Apple Ads ids `asa_org_id`, `asa_campaign_id`, `asa_ad_group_id`, `asa_keyword_id`, `asa_ad_id`, `asa_country`) in the `attribution` field of `/verify` and `/restore`. The server remembers the player's latest values (`user_attribution` table) and uses them for events without a client — renewals and refunds. An attribution integration skips (and logs) events of players without its id: AppsFlyer, Adjust, Tenjin and AppMetrica don't accept them.

- **AppsFlyer** event names: `af_purchase`, `af_subscribe`, `subscription_renew`, `refund` (negative revenue); `expire` isn't sent by default. `event_names` renames or disables them (an empty string disables).
- **Tenjin**: purchases, subscription starts and renewals go to the Purchase API, refunds with a negative price; with `send_expire` expirations go to the Event API as `subscription_expire`.
- **Adjust**: each event kind is sent only when it has an event token (create the events in the Adjust dashboard). Purchases, subscription starts and renewals carry `revenue` and `currency`; refunds go without revenue (Adjust takes no negative amounts) and with `refund_amount` in the callback parameters. Sandbox purchases, when enabled, go to the Adjust sandbox environment.
- **AppMetrica**: In-App Revenue import with the types `one_time_purchase`, `subscription_started`, `subscription_renewed`, `refund`, `expired`; the test event is a custom `purser_test` event. AppMetrica only accepts device ids its SDK already sent, and events at most 14 days old.
- **Webhook**: `POST url` with the event as JSON (`kind`, `app_id`, `product_id`, `price_micros`, `currency`, `user_id`, `transaction_id`, `environment`, `attribution`, …) and the headers `X-Billing-Event: <kind>` and `X-Billing-Signature: hex(HMAC-SHA256(secret, body))`. Answer 2xx; other answers are retried as above.

- **Cloud Storage**: every event becomes one object `{folder}/dt=YYYY-MM-DD/{time}-{kind}-{transaction}.json` holding one JSON line (the webhook body), uploaded with the service account's OAuth token. Point a BigQuery external table at `gs://bucket/folder/*` (NEWLINE_DELIMITED_JSON, hive partitioning on `dt`) or load the files into any warehouse. The service account needs Storage Object Creator on the bucket.
- **Apple Ads** has no server API for purchase events. With `collectAppleAdsAttribution` the Unity SDK resolves the install's campaign on the device (AdServices token → Apple's attribution API, once per install) and sends the `asa_*` ids, which reach the webhook and Cloud Storage exports.

Revenue is the store price (Apple, Google subscriptions) or the localized price sent by the client (Google one-time products), i.e. before the store's commission.

Test the setup without a purchase (like “Send test event” in Adapty): Integrations → **Send test**, or

```bash
spacetime call <your-billing-db> admin_test_integration '"{\"app_id\":\"com.example.mygame\",\"kind\":\"appsflyer\",\"attribution\":{\"appsflyer_id\":\"<id from the device>\"}}"'
spacetime logs <your-billing-db>
```

Adding a new integration:
1. A constant in `purser_core::integrations::integration_kind`.
2. A settings struct and a `build(event, settings, key) -> Built` function (pure, with tests) in `core/src/integrations.rs`.
3. Branches in `build_call`, `validate_settings` and `secret_for` in `spacetimedb/src/integrations.rs`, and a card in `admin-ui/src/pages/integrations.tsx`.

## Client integration

**Unity:** the package [`unity/com.yevhenii.purser`](../unity/com.yevhenii.purser/README.md) on Unity IAP 5. Other clients (GameMaker, …): see [docs/game-integration.md](../docs/game-integration.md#client-integration).

## Build, tests, publishing

```bash
cargo test -p purser-core
cargo test -p purser
cargo build -p purser --target wasm32-unknown-unknown --release
spacetime publish --bin-path target/wasm32-unknown-unknown/release/purser.wasm <your-billing-db>
```

`cargo test -p purser` runs the module's own tests on `spacetimedb::test_utils` (SpacetimeDB 2.12+). They use an in-memory database, and an HTTP mock stands in for Google Play, the game database and the FX API, so no server or keys are needed. The tests cover grant and consume, idempotent repeated `/verify`, worker retries with backoff, protecting consumables from another account, `/restore` onto a new `user_id`, refunds via RTDN, admin permission checks, the setup check, the product import and the revenue aggregates. Test code: [`spacetimedb/src/tests.rs`](spacetimedb/src/tests.rs).

The test build pulls in `spacetimedb-core`, which builds OpenSSL from source. On Windows this needs a full Perl: the Perl from Git Bash won't do. [Strawberry Perl](https://strawberryperl.com/) works:

```bash
OPENSSL_SRC_PERL=C:/Strawberry/perl/bin/perl.exe cargo test -p purser
```

Dependency versions in `Cargo.lock` come from SpacetimeDB 2.12.0's own lockfile; the upgrade to 2.14.1 only updated the `spacetimedb*` crates (`cargo update -p spacetimedb --precise <version>`). Don't run a full `cargo update`: newer `icu_*` crates are incompatible with `temporal_rs`, which `spacetimedb-core` depends on.

Locally: `spacetime start`, then `spacetime publish --server local … billing-local`. Note that a local server can't deliver to private addresses.

### Protection against double grants

The fact of a grant is stored in the `purchase` ledger (`verified` → `granted` → `completed`). The server consumes in Google only after granting, so every consumed purchase granted by this server has a ledger row. If Google reports a consumable as already consumed but there's no row, someone else granted it: a previous billing server migrated without its ledger, Adapty, or the client itself. Such a purchase is granted neither via `/verify` (response `403 forbidden`, `[ERR_CONSUMED_ELSEWHERE]`) nor via RTDN. This way a resent old `purchase_token` won't grant the reward a second time. Disabled by the app's `client_consumes` flag.

Apple and Amazon have no consumed flag, so there double grants are prevented only by migrating the ledger and by the game's idempotency on `transaction_id`.

### Moving to another server

See [docs/migrating-from-adapty.md](../docs/migrating-from-adapty.md#moving-the-ledger-to-another-server).

### Self-hosted server

See [docs/self-hosting.md](../docs/self-hosting.md).

### Useful queries

```bash
spacetime sql <your-billing-db> "SELECT id, status, attempts, last_error FROM purchase WHERE next_attempt_at_micros > 0"
spacetime sql <your-billing-db> "SELECT * FROM purchase_log WHERE purchase_id = 'google:GPA.1234-5678-9012-34567'"
spacetime logs <your-billing-db> -f
```
