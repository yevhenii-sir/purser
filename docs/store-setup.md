# Store setup

What to create in Google Play Console, App Store Connect, the Amazon Developer Console, Steamworks, Xsolla Publisher Account and the Stripe Dashboard. Each store an app sells in is one **build** of the app (same app key): a game on Google Play and the Amazon Appstore has two Android builds. The admin panel's **Add an app** wizard asks for these keys and finishes with the URLs to paste back into the stores; the app's **Setup** tab tells you what is still missing, and its **Keys** tab where every key comes from. Keys shared by several apps are stored once per developer account (Settings → Developer accounts).

Every build has its developer account (the app's Delivery tab, or the wizard's first step): apps published from different Google Play / App Store Connect accounts, or a game whose iOS build is published by someone else, each use their own account's keys. Keep store keys out of `default` once you have more than one account: a build whose account has no key would fall back to another account's key.

## Google Play

### Service account

1. Google Cloud console → the project linked to Play Console → **IAM & Admin → Service accounts → Create**. No roles are needed in Google Cloud.
2. On the account: **Keys → Add key → JSON**. This file is the `google_service_account_json` secret.
3. Play Console → **Users and permissions → Invite new users** → the service account's email (`…@….iam.gserviceaccount.com`). For the app (or the whole account) grant:
   - **View app information and download bulk reports** — product import;
   - **View financial data, orders, and cancellation survey responses** — purchase lookups;
   - **Manage orders and subscriptions** — consume, acknowledge.

New permissions can take up to 24 hours to apply. Until then the setup check reports *Google Play rejected the service account*.

### Real-time developer notifications (RTDN)

Renewals, refunds and expirations arrive through Pub/Sub:

1. Google Cloud → **Pub/Sub → Create topic** (e.g. `play-billing`). Give `google-play-developer-notifications@system.gserviceaccount.com` the **Pub/Sub Publisher** role on the topic.
2. **Create subscription** on the topic → delivery type **Push** → endpoint: the RTDN URL from the wizard or the app's Setup tab, `https://<host>/v1/database/<db>/route/notify/google?secret=<google_rtdn_secret>`.
3. Play Console → the app → **Monetization setup → Real-time developer notifications** → topic `projects/<project>/topics/play-billing` → **Send test notification**.

The setup check shows *Test notification received* when it arrives, and *arrive with a wrong secret* when the secret in the URL does not match. Lost the URL? The app's Setup tab → **New RTDN secret** generates a new one and shows the full URL once.

## App Store

### In-App Purchase key

App Store Connect → **Users and Access → Integrations → In-App Purchase → Generate**. You get:

- **Issuer ID** (top of the page) → `apple_issuer_id`;
- **Key ID** → `apple_key_id`;
- the `.p8` file (downloadable once) → `apple_private_key_p8`.

One key works for every app of the account; store the secrets on the account scope.

### Catalog import (optional)

**Import from App Store** on the app's Products tab (and in the new app wizard) lists the app's in-app purchases and subscriptions from the App Store Connect API. That API does not accept the In-App Purchase key: create a **Team key** in App Store Connect → **Users and Access → Integrations → App Store Connect API → Team Keys** with the **App Manager** role (any role that can see in-app purchases), then store on the account scope:

- **Key ID** → `apple_connect_key_id`;
- the `.p8` file → `apple_connect_private_key_p8`.

The Issuer ID is the same as for the In-App Purchase key (`apple_issuer_id`). The import knows consumables, non-consumables and auto-renewable subscriptions (with their period); a non-renewing subscription comes in as a consumable. Prices are not imported (the App Store reports the price of every purchase); existing products keep their configured price. The key is only used for this read-only listing.

### App Store Server Notifications

App Store Connect → the app → **App Information → App Store Server Notifications**: **Version 2**, the same URL for Production and Sandbox: `https://<host>/v1/database/<db>/route/notify/apple` (add `?secret=…` if you set `apple_notification_secret`). The server accepts only notifications signed by Apple, so the secret is optional.

Each setup check asks Apple to send a test notification; the next check shows whether Apple delivered it.

### Refund requests

When a player asks Apple for a refund, Apple sends a `CONSUMPTION_REQUEST` notification and gives the developer 12 hours to say whether the purchase was delivered and used. Apple takes the answer into account when it decides. Turn it on per build: the app's **Delivery** tab → Builds → **Refund requests** (`game_app.apple_refund_mode`):

| Mode | What is sent |
|---|---|
| Don't answer (`off`, default) | nothing |
| Send consumption data (`inform`) | delivery status and, for consumables, how much was used |
| Send data and recommend an outcome (`prefer`) | the same plus `refundPreference` |

What the server answers, from the ledger:

| Purchase | `deliveryStatus` | `consumptionPercentage` | `refundPreference` (`prefer`) |
|---|---|---|---|
| not delivered to the game | `UNDELIVERED_SERVER_OUTAGE` while delivery is still retried, else `UNDELIVERED_OTHER` | 0 | `GRANT_FULL` |
| delivered consumable | `DELIVERED` | 100% (the reward went to the game in full) | `DECLINE` |
| delivered non-consumable | `DELIVERED` | - | `DECLINE` |
| subscription | `DELIVERED` | - (Apple computes it) | `GRANT_PRORATED` |

`sampleContentProvided` is always `false`. A purchase that is already refunded is not answered.

**Consent.** Apple only accepts the answer for players who agreed to share this data (`customerConsented`); for the others the server stays silent. Ask in the game with your own prompt (not the App Tracking Transparency one: Apple says the two are unrelated), say that some purchase data goes to Apple to review refund requests and that the player can withdraw, and call `SetRefundDataConsent(true/false)` in the Unity package. The answer travels with the next purchase verification or restore (`attribution.refund_data_consent`) and is kept per player. Disclose the data in the app's privacy details on App Store Connect.

The answer is queued when the notification arrives and retried on network errors and 5xx until the 12 hours run out. The purchase history in the admin page shows `refund_requested`, then `consumption_sent` (with the status and outcome sent), `consumption_skipped` (no consent) or `consumption_failed`.

## Amazon Appstore

The server checks receipts with Amazon's [Receipt Verification Service](https://developer.amazon.com/docs/in-app-purchasing/iap-rvs-for-android-apps.html) (RVS) and learns about refunds and renewals from [Real-Time Notifications](https://developer.amazon.com/docs/in-app-purchasing/real-time-notifications.html) (RTN).

### The build

Add a build with platform **Amazon** (wizard → Add a build). Its app id only has to be unique on the server: RVS receipts carry no package name. When the Google Play build already uses the package name, use e.g. `com.example.mygame.amazon`, and use the same value for the `amazon` build in the Unity `BillingConfig`. Products use the Amazon SKUs; for subscriptions the term SKU (the one the client buys) or the parent SKU.

### Shared key

Developer Console → **Apps & Services → Shared Key** → copy it into `amazon_shared_secret`. One key serves every app of the developer account: store it on the account scope. The setup check calls RVS with a made-up receipt to see whether Amazon accepts the key.

### Real-Time Notifications

1. Developer Console → the app → **App Services → Real-Time Notifications → Add an Endpoint** → the URL from the app's Setup tab: `https://<host>/v1/database/<db>/route/notify/amazon?app=<app id>`.
2. Amazon sends a confirmation message to the endpoint. Run the setup check: it shows the **confirmation token**. Paste it into the console's *Confirmation token* field and confirm (the token is valid for 48 hours; *Resend* gets a new one).
3. The endpoint status turns *Verified*; notifications appear as *Last notification* in the setup check.
4. The setup check shows the SNS topic the messages come from. Copy it into **SNS topic** on the app's Delivery tab: from then on only that topic is accepted (otherwise any AWS account could send signed messages to the endpoint).

Messages are verified with their Amazon SNS signature; a wrong signature shows up as *notifications are rejected*. Without RTN, Amazon subscriptions are re-checked by the server shortly after each renewal date, and refunds of consumables and entitlements are not noticed.

### Testing

- **App Tester** (sandbox mode): enable **Sandbox purchases** on the build. The server then asks the RVS Cloud Sandbox when production does not know the receipt. Sandbox accepts any shared key, so keep this off for live builds unless you need it.
- **Live App Testing**: receipts are marked as test transactions and count as sandbox purchases.

### Client

Unity IAP 5 has no Amazon store: the Unity package uses `AmazonStoreAdapter` over Amazon's Appstore SDK Unity plugin (sample "Amazon Appstore"). Other clients post the receipt id as `order_id` and the Amazon user id as `store_user_id` to `/verify/amazon`, then call `notifyFulfillment` once the server answered.

## Xsolla

[Xsolla](https://developers.xsolla.com/) sells on the web and on desktop (Pay Station / Web Shop). There is no client receipt: the server creates a payment token for one item, the player pays in Pay Station, and Xsolla tells the server with [webhooks](https://developers.xsolla.com/webhooks/overview/). Players never see the API key or the secret key.

### The build

Add a build with platform **Web** and store **Xsolla**. Its app id only has to be unique on the server, e.g. `com.example.mygame.web`; use the same value for the `xsolla` build in the Unity `BillingConfig`. Use **one Xsolla project per build**: a project has one webhook URL, and it names the build.

Products use the **item SKUs** of the project's catalog (Publisher Account → the project → **Items catalog**, virtual items or currency packages): the SKU is the product id. Kinds: a consumable for currency packages and consumable items, a non-consumable for items bought once. Prices come from Xsolla's webhooks, so no default price is needed.

### Keys

Store on the build (Keys tab):

- `xsolla_project_id`: the project ID shown in Publisher Account (a number; not secret, kept with the keys so it never leaves the server);
- `xsolla_api_key`: Publisher Account → **Company settings → API keys** (a key for every project of the company: store it on the developer account) or the project's **Project settings → API keys**. The server uses it with the project id (Basic auth `project_id:api_key`) to create payment tokens;
- `xsolla_webhook_secret`: the project's secret key, **Project settings → Webhooks → Secret key**.

The setup check lists the project's virtual items with the key (one call): it fails when Xsolla rejects the project id / key, and warns about products that are not enabled virtual items (bundles and currency packages are not listed by that call).

### Webhooks

1. Publisher Account → the project → **Project settings → Webhooks** → turn webhooks on, URL: the one from the app's Setup tab, `https://<host>/v1/database/<db>/route/notify/xsolla?app=<app id>`.
2. Copy the **secret key** into `xsolla_webhook_secret`.
3. Send a test webhook from the page: a `user_validation` test answers `204` and the setup check shows *Last notification*. (A test payment for a SKU that is not a product of the build answers `500`, so that real payments of products added later are retried by Xsolla.) A wrong secret shows up as *notifications are rejected (invalid signature)*.

Projects created since January 2025 use the combined **`order_paid` / `order_canceled`** webhooks, which carry the payment data; older ones send **`payment` / `refund`** with `order_paid` / `order_canceled` alongside. The server understands both: purchases are keyed by the Xsolla transaction id, so a payment reported twice is granted once. With the separate model keep the `payment` webhook on (an `order_paid` without payment data is acknowledged and waits for it).

**User validation.** Xsolla may ask whether a user exists (`user_validation`). The server has no user list: it accepts any well-formed billing user id (letters, digits, `-_.:`, up to 128 characters) and answers `400 INVALID_USER` otherwise. The checkout puts the player's billing user id into the token, so Xsolla only ever sees ids the game chose.

**Refunds.** `refund` and `order_canceled` revoke the purchase in the game and count as refunds in revenue. A `partial_refund` leaves the purchase granted. `dispute` (chargeback) webhooks are acknowledged without a change; revoke a lost chargeback from the player's page in the admin panel if Xsolla does not send a refund for it.

**Subscriptions** (best effort). `/checkout/xsolla` sells catalog items only: Xsolla subscriptions use their own token with a plan, which this server does not create. A subscription sold another way is still recorded from its webhooks when the product id is the plan's external id: every `payment` with subscription data grants it until `date_next_charge`, and `cancel_subscription` ends it at `date_end` (revoked in the game once that date has passed when the webhook arrives; for a later date the subscription simply ends then, without a revoke event). `create_subscription`, `update_subscription` and `non_renewal_subscription` need no action.

### Checkout

1. The client asks `POST /checkout/xsolla` with the app id, product id (SKU) and user id (optionally `sandbox`, `return_url`, `country`, `language`) and gets `{token, order_id, url}`.
2. It opens `url` (Pay Station) in a browser tab or window.
3. Xsolla calls the webhook; the server grants the purchase (delivery to the game as for every store).
4. The client polls `POST /checkout/xsolla/status` with the `order_id` every few seconds until `success` (or relies on its game server receiving the grant).

Xsolla needs the player's country to pick the currency: pass `country` (ISO 3166-1 alpha-2; the player can change it in Pay Station), otherwise the server forwards the address from `X-Forwarded-For` when the hosting adds one.

**From a website or launcher without Unity.** The same three calls work from any client: your page calls `/checkout/xsolla` (from your own backend or directly from the browser: only public ids travel), redirects to `url` or opens it in a new window, and either polls `/checkout/xsolla/status` or waits for `return_url`. Entitlements of non-consumables and subscriptions are in `POST /entitlements`, shared with the other builds of the game.

**Unity.** `XsollaStoreAdapter` does steps 1–4 (see the package README).

### Testing

Enable **Sandbox purchases** on the build and buy with `sandbox: true` (Unity: `XsollaOptions.Sandbox`): Pay Station opens on `sandbox-secure.xsolla.com` and accepts Xsolla's test cards (see "Testing" in the Pay Station docs). Test payments carry `dry_run = 1` and count as sandbox purchases; a build without sandbox purchases ignores them.
## Steam

Steam in-game purchases are [microtransactions](https://partner.steamgames.com/doc/features/microtransactions/implementation) driven by the server: the game asks the server to start an order, Steam shows its overlay, and the server captures the money. The publisher Web API key stays on the server (it must never ship with the game). Steam has no purchase notifications: the server polls Steam's transaction report for refunds and chargebacks.

### The build

Add a build with platform **Steam**. Its app id is the numeric **Steam App ID** (e.g. `480`). A second build of the same App ID (say a sandbox build next to the live one) can use any unique app id and name the App ID with `admin_set_steam_build` (`{"app_id": "com.example.mygame.steam-test", "steam_app_id": "480"}`).

### Publisher Web API key

Steamworks → **Users & Permissions → Manage Groups** → the group that owns the App ID → **Create WebAPI Key**. Copy it into `steam_publisher_key`; one key serves every app of the group, so store it on the account scope. The key needs access to the App ID (microtransactions). If the group locks keys to IP addresses, either remove the lock or add the addresses the billing server calls from; otherwise Steam answers 403 and the setup check reports *the publisher key was rejected or is not allowed from this IP*.

The server sends the key in the `x-webapi-key` header of `https://partner.steam-api.com/ISteamMicroTxn/...` calls, never in a URL.

### Items and prices

Every product of the build is sold as one Steam item (quantity 1):

- **Item id**: Steam needs a 32-bit number per item. By default it is a stable hash of the product id; set `item_id` to use your own (e.g. the ids your Steam store packages use).
- **Description**: shown in the Steam overlay, up to 128 characters; by default the product id. Set a readable one with `description`.
- **Price**: Steam charges in the player's wallet currency, reported by `GetUserInfo` at checkout. The server uses the item's price in that currency if you set one (`prices`, in hundredths of the currency unit for every currency: `{"USD": 199, "EUR": 179, "JPY": 30000}`); otherwise the product's catalog price (`default_price_micros` + `currency`), which Steam converts at the current rate and shows converted. Some currencies only accept whole units (Steam rejects e.g. a UAH price of 10.50): give those an explicit price.

Set them with `admin_upsert_steam_items` (or `steam.json` for `seed.py`):

```json
{
  "builds": [{ "app_id": "480", "sandbox": true }],
  "items": [
    { "app_id": "480", "product_id": "gems_100", "description": "100 Gems", "prices": { "USD": 99, "EUR": 99 } },
    { "app_id": "480", "product_id": "vip_month", "description": "VIP (monthly)", "period": "Month", "frequency": 1 }
  ]
}
```

The setup check fails for products that have no price at all.

### Purchase flow

1. The game calls `POST /checkout/steam` with the product, the player's user id, SteamID64 (`ISteamUser::GetSteamID`), language (`ISteamApps::GetCurrentGameLanguage`) and a Web API auth ticket (`ISteamUser::GetAuthTicketForWebApi("purser")`, hex). The server checks that the ticket belongs to the SteamID, calls `GetUserInfo` and `InitTxn` and returns the order id. Steam shows the overlay.
2. The game receives `MicroTxnAuthorizationResponse_t` and, when the player authorized, calls `POST /verify/steam` with the order id. The server calls `FinalizeTxn` and `QueryTxn`, records the purchase and delivers the reward like any other store.
3. If the game dies in between, the worker finds the authorized order through `QueryTxn` and finishes it; the player gets the reward through delivery (or `/entitlements` / restore for owned products). Orders the player never authorizes are given up after a day.

### Refunds and chargebacks

Every 5 minutes per build the worker asks `GetReport` (type `GAMESALES`) for transactions changed since the last call and revokes purchases that became `Refunded`, `Chargedback`, `RefundedSuspectedFraud` or `RefundedFriendlyFraud` (and partial refunds of the item). The setup check shows when the report was last read.

### Subscriptions

A `subscription` product is sold as a Steam-billed recurring agreement (`billingtype Steam`, `period` Day / Week / Month / Year, `frequency` periods between charges, default monthly; the renewal amount is the first charge). The ledger keeps one row per agreement: it runs until the day after `nextpayment` from `GetUserAgreementInfo`, and the worker asks again after that day; a renewal moves the expiry on (the game gets `grant` again), a canceled agreement ends when its paid period does. Renewal payments are not added to revenue as separate transactions. Steam allows one active agreement per player and game: a new agreement replaces the previous one.

### Auth tickets

Without a ticket, anyone who knows a player's SteamID could start orders in their name. Once every build of the game sends tickets (the Unity package does when the bridge implements `ISteamWebApiTicketProvider`, as the sample does), set `"require_ticket": true` with `admin_set_steam_build`: checkouts without a valid ticket are then refused. The setup check warns while it is off.

### Testing

Set `"sandbox": true` with `admin_set_steam_build` and enable **Sandbox purchases** on the build: every call then goes to `ISteamMicroTxnSandbox`, no money moves, and purchases are recorded as sandbox. Turn it off for the release build (the setup check warns while it is on).

### Client

The Unity package has `SteamStoreAdapter`, with a Steamworks.NET bridge in the sample "Steam". Other clients call `/checkout/steam`, wait for `MicroTxnAuthorizationResponse_t` with the returned order id, then call `/verify/steam`.

## Stripe

Web builds can sell through [Stripe Checkout](https://docs.stripe.com/payments/checkout). The server creates the Checkout Session, the player pays on Stripe's page, and Stripe's [webhooks](https://docs.stripe.com/webhooks) tell the server about payments, renewals, cancellations, refunds and disputes. There is no client receipt: the webhook is the source of truth, and `/verify/stripe` only lets the client get its answer right after the redirect.

### The build and its products

1. Add a build with platform **Web** and store **Stripe** (wizard → Add a build). Its app id only has to be unique, e.g. `com.example.mygame.web` (the `stripe` build in the Unity `BillingConfig`; a game selling through Stripe and Xsolla has a build for each).
2. Stripe Dashboard → **Product catalog** → a product per item with a **Price**: one-time for consumables and non-consumables, recurring for subscriptions.
3. Add the products to the build with the **Price id** (`price_…`) as the product id and the matching kind. Test mode and live mode have different Price ids: use a test build (sandbox purchases on) and a live build, or change the ids when going live. Entitlements of a Stripe build name the Price ids; the Unity adapter maps your product ids to them (`StripeCheckoutOptions.PriceIds`).

The setup check fetches every product's Price (up to 8 per run) and reports missing, archived or wrong-type Prices.

### Keys

- `stripe_secret_key`: Developers → **API keys** → the secret key (`sk_live_…` / `sk_test_…`). One per Stripe account: store it on the developer account. A [restricted key](https://docs.stripe.com/keys/restricted-api-keys) (`rk_…`) works too with **Checkout Sessions: Write** and **Prices, Subscriptions, Invoices: Read**.
- A test-mode key makes sandbox purchases: enable **Sandbox purchases** on the build, or every purchase is ignored (the setup check fails then).

### Webhook endpoint

1. Developers → **Webhooks** → **Add destination** → events from **your account**, payload style **Snapshot**, any API version → endpoint URL from the app's Setup tab: `https://<host>/v1/database/<db>/route/notify/stripe?app=<app id>` (one endpoint per build).
2. Events:
   - `checkout.session.completed`, `checkout.session.async_payment_succeeded`
   - `invoice.paid` (or `invoice.payment_succeeded`)
   - `customer.subscription.updated`, `customer.subscription.deleted`
   - `charge.refunded`, `charge.dispute.created`
3. Copy the endpoint's **Signing secret** (`whsec_…`) into `stripe_webhook_secret` of the build.

Every webhook is checked with its `Stripe-Signature` (5 minutes tolerance); a wrong secret shows up in the setup check as *notifications are rejected*. Stripe retries failed deliveries for three days; retries and duplicates never grant twice. Partial refunds keep the purchase; a full refund or a dispute revokes it. Plan changes in the customer portal are not followed (the subscription keeps the product it was bought as): turn off switching plans there.

### Test

`sk_test_` key, Sandbox purchases on, a test card (`4242 4242 4242 4242`). The ledger row is a `sandbox` purchase with the Stripe transaction id (`stripe:pi_…` for one-time purchases, `stripe:sub_…` for the first subscription period, `stripe:in_…` for renewals). [`stripe trigger`](https://docs.stripe.com/cli/trigger) events have none of the server's metadata and are ignored: test through `/checkout/stripe`.

### Revenue

Stripe reports the price paid (in the currency the customer paid; zero-decimal currencies like JPY are handled). Its processing fee is not a store commission: set the proceeds estimate for Stripe builds with the per-build commission, e.g. `admin_set_setting("store_commission_by_app", "{\"com.example.mygame.web\": 3}")` (≈ card fee) or `0` (Revenue → Store fee in the admin page). Taxes collected by Stripe Tax are part of the amount.

### Client

- **Unity**: `StripeStoreAdapter` (see the package README) does the steps below: checkout, opens the page, polls `/verify/stripe` until the session is granted.
- **Any website** (no Unity), with the player's id known to the page:
  1. `POST /checkout/stripe` `{"app_id", "product_id": "price_…", "user_id", "success_url": "https://game.example/thanks?session={CHECKOUT_SESSION_ID}", "cancel_url"}` → redirect to `url`.
  2. On the success page: `POST /verify/stripe` `{"app_id", "user_id", "session_id"}` → `success` once paid (`pending_or_canceled` while not: ask again in a few seconds), or read `/entitlements`.
  3. The game backend receives `grant` / `revoke` as for any store (or `delivery = none`: the page grants on `success`).

## Creating products from the admin page

**Create in store** on an app's Products tab creates the app's products in Google Play, the App Store, Stripe and Xsolla. You pick:

- the products and the builds to create them in;
- an English title and description;
- the price and currency;
- for subscriptions, the billing period.

The title, description, period and App Store subscription group start from the product's **store listing** (edited in the product dialog, and saved again after each product is created), so they are typed once for every store and build.

**Check** shows, for every build and product, whether it will be created. A product the store already has is skipped and can be added to the build's products instead. Each product then runs as a job in the background; the list under the products shows its progress and errors. A finished product is added to the build's products.

The store converts the price to the other countries:

- **Google Play.**
  - Prices: Google converts them to every region.
  - Activation: one-time products get an active purchase option `default`; subscriptions get an active auto-renewing base plan named after the period (e.g. `p1m`).
  - Permissions: the service account needs **Manage store presence**.
  - Before you start: the app needs a build with Play Billing uploaded to any track.
- **App Store.**
  - Key: needs the [App Store Connect API Team key](#catalog-import-optional) (App Manager or Admin role).
  - Prices: the base price is the nearest App Store price point in the territory of the currency (USD → United States, EUR → Germany, …; the job notes when it differs). Apple fills in the other territories for in-app purchases; for subscriptions the job sets every territory's equalized price, which takes about 180 calls.
  - Subscriptions go to the subscription group you name, which is created if missing.
  - Left for App Store Connect: the review screenshot and the review submission. The first in-app purchase of an app must be submitted with an app version.
- **Stripe.**
  - Creates a Product with a default Price (recurring for subscriptions); one request, with an idempotency key.
  - The new **Price id** becomes the build's product id: put it into the Unity config's store ids.
  - Permissions: a restricted key needs Products: Write and Prices: Write.
- **Xsolla.**
  - Creates an enabled virtual item with the SKU = product id and one price.
  - Subscriptions are not supported.
- **Amazon Appstore and Steam:** not supported. Amazon has no API for in-app items (use the Developer Console or its CSV upload). Steam items are defined by the game itself.

## Sandbox and testers

Enable **Sandbox purchases** for the builds on the app's Delivery tab for App Review, TestFlight and license testers. Sandbox purchases are excluded from revenue by default (Revenue → *Include sandbox*) and from integrations unless their *Send sandbox / test purchases* option is on.
