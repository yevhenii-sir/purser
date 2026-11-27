# Store setup

This page lists what to create in each store's console: Google Play Console, App Store Connect, the Amazon Developer Console, Samsung Seller Portal, Steamworks, Xsolla Publisher Account and the Stripe Dashboard. It also covers Huawei AppGallery, Meta Quest and Telegram Stars. Jump to the store you sell in.

Each store an app sells in is one **build** of the app (same app key). For example, a game on Google Play and the Amazon Appstore has two Android builds.

Where to enter the keys and URLs:

- **Add an app** wizard (admin panel): asks for these keys and finishes with the URLs to paste back into the stores.
- The app's **Setup** tab: tells you what is still missing.
- The app's **Keys** tab: tells you where every key comes from.
- **Settings → Developer accounts**: keys shared by several apps are stored once per developer account.

Every build has its developer account (the app's Delivery tab, or the wizard's first step). Apps published from different Google Play / App Store Connect accounts each use their own account's keys. So does a game whose iOS build is published by someone else.

!!! warning
    Keep store keys out of `default` once you have more than one account. A build whose account has no key would fall back to another account's key.

## Google Play

A Google Play build needs a service account and real-time developer notifications. The server can set up the Google Cloud side for you, or you can do it by hand in the sections after the quick setup.

### Quick setup with Google Cloud

The server can set up the Google Cloud side with your own Google login. It uses a short-lived access token from the [Google Cloud CLI](https://cloud.google.com/sdk/docs/install) (`gcloud auth login` with an account that may change the project).

Before you start:

- **Builds**: the builds must exist already (wizard or `seed.py`).
- **Public address**: the server must be reachable over https under a public host name. That is the `public_base_url` setting, else the address the admin page is opened at.
- **Role**: the token's account needs the **Owner** role on the project, or Service Usage Admin, Service Account Admin, Service Account Key Admin and Pub/Sub Admin.

Run the setup from either place:

- **Admin page**: the app's **Setup** tab → **Set up with Google Cloud** → the project id and the output of `gcloud auth print-access-token`. The token is used for this one run and is not stored.
- **Terminal**: `server/setup_google.py` gets the token from `gcloud` itself and does the same:

```bash
cd server
python setup_google.py --project <your-cloud-project> --dry-run   # what would change
python setup_google.py --project <your-cloud-project>             # every Google Play build, or --app-id com.example.mygame
```

The setup does the following:

- It enables the IAM, Pub/Sub and Google Play Developer APIs.
- It creates the service account `purser-billing@<project>.iam.gserviceaccount.com` and its JSON key. The key is stored as `google_service_account_json` and never written to a file. `--new-key` replaces it.
- It creates the topic, with `google-play-developer-notifications@system.gserviceaccount.com` as **Pub/Sub Publisher**. Existing bindings are kept.
- Per build, it creates the RTDN push subscription, authenticated as the build's service account. An existing subscription without authentication, or with another account or audience, is changed.

Steps already done are skipped, so it is safe to run again.

The setup ends with what only Play Console can do:

1. Invite the service account (step 3 of [Service account](#service-account)).
2. Set the RTDN topic, then press **Send test notification** (step 3 of [RTDN](#real-time-developer-notifications-rtdn)).

Projects created before 8 April 2021 also need the grant of step 2 of [RTDN](#real-time-developer-notifications-rtdn).

!!! note
    Organizations created after May 3, 2024 forbid service account keys by default (`iam.managed.disableServiceAccountKeyCreation`). An organization policy administrator has to turn the policy off for this project. Then run the setup again.

### Service account

1. Open the Google Cloud console → the project linked to Play Console → **IAM & Admin → Service accounts → Create**. No roles are needed in Google Cloud.
2. On the account, open **Keys → Add key → JSON**. This file is the `google_service_account_json` secret.
3. In Play Console, open **Users and permissions → Invite new users** and enter the service account's email (`…@….iam.gserviceaccount.com`). Grant it the permissions below, for the app or the whole account.

| Permission | What it is for |
|---|---|
| **View app information and download bulk reports** | product import |
| **View financial data, orders, and cancellation survey responses** | purchase lookups; the price, tax and country of each order (without it, revenue uses the client's or the catalog price); the voided purchases the worker reads every 6 hours (refunds a notification missed) |
| **Manage orders and subscriptions** | consume, acknowledge |

New permissions can take up to 24 hours to apply. Until then the setup check reports *Google Play rejected the service account*.

### Real-time developer notifications (RTDN)

Renewals, refunds and expirations arrive through Pub/Sub. Set it up in three steps:

1. In Google Cloud, open **Pub/Sub → Create topic** (e.g. `play-billing`). Give `google-play-developer-notifications@system.gserviceaccount.com` the **Pub/Sub Publisher** role on the topic.
2. Open **Create subscription** on the topic and set:
    - delivery type: **Push**;
    - endpoint: the RTDN URL from the wizard or the app's Setup tab, `https://<host>/v1/database/<db>/route/notify/google` (no secret in the URL);
    - **Enable authentication**: service account = the `client_email` of the build's `google_service_account_json`, audience = `purser-rtdn:<app_id>` (both are on the Setup tab).

    Your Google account needs **Service Account User** on that service account (Owners have it).

    Projects created before 8 April 2021: grant `roles/iam.serviceAccountTokenCreator` on the project to the Pub/Sub service agent `service-<PROJECT_NUMBER>@gcp-sa-pubsub.iam.gserviceaccount.com`. Otherwise Pub/Sub cannot sign its pushes.
3. In Play Console, open the app → **Monetization setup → Real-time developer notifications**. Set the topic `projects/<project>/topics/play-billing` and press **Send test notification**.

Pub/Sub then sends `Authorization: Bearer <token>`, a token Google signs for the service account and the audience. The server accepts a push only when all of these hold:

- Google's signature checks out (keys from `https://www.googleapis.com/oauth2/v3/certs`, cached, looked up again at most once a minute);
- the token names the build's service account (verified email) and audience;
- the token is current.

The audience does not name the server's address, so a [moved server](moving-servers.md) accepts the same tokens.

The setup check shows *Test notification received* when a test arrives. When a push was refused, it shows the reason: no token (authentication is off), or another service account or audience.

## App Store

An App Store build needs the In-App Purchase key. An App Store Connect API Team key is optional. It adds catalog import, setting the notification URLs from the admin page, and creating products.

### In-App Purchase key

In App Store Connect, open **Users and Access → Integrations → In-App Purchase → Generate**. You get:

- **Issuer ID** (top of the page) → `apple_issuer_id`;
- **Key ID** → `apple_key_id`;
- the `.p8` file (downloadable once) → `apple_private_key_p8`.

One key works for every app of the account. Store the secrets on the account scope.

### Catalog import (optional)

**Import from App Store** on the app's Products tab (and in the new app wizard) lists the app's in-app purchases and subscriptions from the App Store Connect API. That API does not accept the In-App Purchase key, so it needs a Team key:

1. In App Store Connect, open **Users and Access → Integrations → App Store Connect API → Team Keys**.
2. Create a **Team key** with the **App Manager** role (any role that can see in-app purchases).
3. Store these on the account scope:
    - **Key ID** → `apple_connect_key_id`;
    - the `.p8` file → `apple_connect_private_key_p8`.

The Issuer ID is the same as for the In-App Purchase key (`apple_issuer_id`).

What the import brings in:

- consumables, non-consumables and auto-renewable subscriptions (with their period);
- a non-renewing subscription, which comes in as a consumable.

Prices are not imported, because the App Store reports the price of every purchase. Existing products keep their configured price.

The same key lets the server set the notification URLs (next section) and create products ([below](#creating-products-from-the-admin-page)).

### App Store Server Notifications

**With the Team key.** Press **Set in App Store Connect** next to the build on the app's **Setup** tab (`admin_setup_apple_notifications`). The server sets both URLs, version 2, with `apple_notification_secret` if you set one. The setup check then compares the URLs App Store Connect has with this server's.

**By hand.**

1. In App Store Connect, open the app → **App Information → App Store Server Notifications**.
2. Choose **Version 2**.
3. Enter the same URL for Production and Sandbox: `https://<host>/v1/database/<db>/route/notify/apple`. Add `?secret=…` if you set `apple_notification_secret`.

The server accepts only notifications signed by Apple, so the secret is optional.

Each setup check asks Apple to send a test notification. The next check shows whether Apple delivered it.

### Refund requests

When a player asks Apple for a refund, Apple sends a `CONSUMPTION_REQUEST` notification. The developer then has 12 hours to say whether the purchase was delivered and used. Apple takes the answer into account when it decides.

Turn it on per build: the app's **Delivery** tab → Builds → **Refund requests** (`game_app.apple_refund_mode`).

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

**Consent.** Apple only accepts the answer for players who agreed to share this data (`customerConsented`). For the others, the server stays silent. To collect consent:

1. Ask in the game with your own prompt. Do not use the App Tracking Transparency prompt: Apple says the two are unrelated.
2. In the prompt, say that some purchase data goes to Apple to review refund requests, and that the player can withdraw.
3. Call `SetRefundDataConsent(true/false)` in the Unity package. The answer travels with the next purchase verification or restore (`attribution.refund_data_consent`) and is kept per player.
4. Disclose the data in the app's privacy details on App Store Connect.

The answer to Apple is queued when the notification arrives. It is retried on network errors and 5xx until the 12 hours run out. The purchase history in the admin page shows `refund_requested`, then one of:

- `consumption_sent` (with the status and outcome sent);
- `consumption_skipped` (no consent);
- `consumption_failed`.

## Amazon Appstore

The server checks receipts with Amazon's [Receipt Verification Service](https://developer.amazon.com/docs/in-app-purchasing/iap-rvs-for-android-apps.html) (RVS). It learns about refunds and renewals from [Real-Time Notifications](https://developer.amazon.com/docs/in-app-purchasing/real-time-notifications.html) (RTN).

### The build

Add a build with platform **Amazon** (wizard → Add a build). Its app id is the package name, the same as the Google Play build's: a build is its store and its id, so the two do not collide. RVS receipts carry no package name, so any id unique among Amazon builds works too; use the same value for the `amazon` build in the Unity `BillingConfig` when it is not the package name.

Products use the Amazon SKUs. For subscriptions, use the term SKU (the one the client buys) or the parent SKU.

### Shared key

1. In the Developer Console, open **Apps & Services → Shared Key**.
2. Copy it into `amazon_shared_secret`. One key serves every app of the developer account, so store it on the account scope.

The setup check calls RVS with a made-up receipt to see whether Amazon accepts the key.

### Real-Time Notifications

1. In the Developer Console, open the app → **App Services → Real-Time Notifications → Add an Endpoint**. Enter the URL from the app's Setup tab: `https://<host>/v1/database/<db>/route/notify/amazon?app=<app id>`.
2. Amazon sends a confirmation message to the endpoint. Run the setup check: it shows the **confirmation token**. Paste it into the console's *Confirmation token* field and confirm. The token is valid for 48 hours. *Resend* gets a new one.
3. Check that the endpoint status turns *Verified*. Notifications appear as *Last notification* in the setup check.
4. Copy the SNS topic the setup check shows (the topic the messages come from) into **SNS topic** on the app's Delivery tab. From then on, only that topic is accepted. Otherwise any AWS account could send signed messages to the endpoint.

Messages are verified with their Amazon SNS signature. A wrong signature shows up as *notifications are rejected*.

!!! warning
    Without RTN, Amazon subscriptions are re-checked by the server shortly after each renewal date, and refunds of consumables and entitlements are not noticed.

### Testing

- **App Tester** (sandbox mode): enable **Sandbox purchases** on the build. The server then asks the RVS Cloud Sandbox when production does not know the receipt. Sandbox accepts any shared key, so keep this off for live builds unless you need it.
- **Live App Testing**: receipts are marked as test transactions and count as sandbox purchases.

### Client

Unity IAP 5 has no Amazon store. The Unity package uses `AmazonStoreAdapter` over Amazon's Appstore SDK Unity plugin (sample "Amazon Appstore").

Other clients:

1. Post the receipt id as `order_id` and the Amazon user id as `store_user_id` to `/verify/amazon`.
2. Call `notifyFulfillment` once the server answered.

## Huawei AppGallery

Huawei devices without Google services buy through HMS In-App Purchases. The server looks purchases up with an app-level token: one-time products in Huawei's IAP Order service, subscriptions in its Subscription service. It learns about refunds and renewals from Huawei's server notifications. Each notification only says which purchase to look up again.

### The build

Add a build with platform **Huawei** (wizard → Add a build). Its app id is the package name, the same as the Google Play build's is fine (a build is its store and its id).

Products use the AppGallery product ids. Huawei reports the price paid (×100 in the buyer's currency) and the buyer's country.

### Keys

In AppGallery Connect, open the app → **App information**. Copy these values of the app's OAuth client:

- the **App ID** into `huawei_client_id`;
- the **App secret** into `huawei_client_secret`.

The setup check asks Huawei for a token with them.

`huawei_site` (optional, on the account) is the site of the IAP services. Pick it by where the app is sold:

- `europe` (the default), `asia`, `russia` or `china`;
- `carrier` for an app sold through a telecom carrier (purchases with `accountFlag` 1).

### Server notifications

1. Generate the secret (`huawei_notify_secret`) on the Keys tab.
2. In AppGallery Connect, open the app → **In-App Purchases → Settings → Server notification URL** (version **v2**).
3. Enter the URL from the app's Setup tab: `https://<host>/v1/database/<db>/route/notify/huawei?app=<app id>&secret=<huawei_notify_secret>`.

The server does not trust a notification's content. It looks the purchase up again, so a forged notification changes nothing.

!!! warning
    Without notifications, subscriptions are re-checked by the worker when their period ends, and refunds of one-time products are not noticed.

### Client

Unity IAP has no Huawei store. The Unity package uses `HuaweiStoreAdapter` over a bridge to Huawei's IAP SDK (sample "Huawei AppGallery" for the HMS Unity Plugin).

Other clients:

1. Post the `purchaseToken` as `purchase_token` to `/verify/huawei`. For subscriptions, also post the `subscriptionId` as `order_id`.
2. After a success, consume the purchase (`consumeOwnedPurchase`).

Sandbox purchases (`purchaseType` 0, test accounts) need **Sandbox purchases** on the build.

## Samsung Galaxy Store

Galaxy devices buy through Samsung In-App Purchase. The server:

- checks one-time products with Samsung's receipt API (`iap/v6/receipt`, by purchase id);
- reads subscriptions with the Galaxy Store Developer API's subscription status;
- consumes consumables and acknowledges subscriptions after the grant (Purchase Acknowledgment API);
- learns about refunds and renewals from Instant Server Notifications. Each only says which purchase to look up again.

### The build

Add an **Android** build with store **Samsung Galaxy Store** (wizard → Add a build). Its app id is the package name, the same as the Google Play build's is fine (a build is its store and its id): Samsung's APIs and receipts name the package. Products use the Seller Portal item ids. Samsung reports the price paid (with tax) and the buyer's country.

### Keys

1. In Seller Portal, open **Assistance → API Service**.
2. Create a service account with the **Publishing & ITEM** service.
3. Copy its **Service Account ID** into `samsung_service_account_id`.
4. Upload the private key as `samsung_private_key`. Samsung shows the key once, when the account is created.

Both keys are stored on the developer account. The server signs a JWT with the key and exchanges it for an access token (`devapi.samsungapps.com/auth/accessToken`). The setup check asks for one.

`samsung_iap_public_key` is the app's **IAP public key** from Seller Portal (PEM or the bare base64 text). Instant Server Notifications are verified with it (RS256).

### Instant Server Notifications

In Seller Portal, open the app → **In App Purchase**. Set the ISN URL of each item to `https://<host>/v1/database/<db>/route/notify/samsung?app=<app id>`. The app's Setup tab shows it.

Samsung's documentation does not say whether the URL may carry a query string. Without `?app=`, the notification goes to the build whose app id is its package name.

Notifications are not trusted for their content: the purchase is looked up again. They must also be signed with the IAP key.

!!! warning
    Without notifications, subscriptions are re-checked by the worker when their period ends, and refunds of one-time products are not noticed.

### Client

Unity IAP has no Samsung store. The Unity package uses `SamsungStoreAdapter` over a bridge to Samsung's IAP Unity plugin (sample "Samsung Galaxy Store").

Other clients post the `purchaseId` as `purchase_token` to `/verify/samsung`, and optionally the `orderId` as `order_id`.

The server consumes and acknowledges. A client whose build grants on the client (`delivery` = `none`) consumes a consumable itself once the reward is granted.

Test purchases (`OPERATION_MODE_TEST`, licensed testers) are sandbox purchases. They need **Sandbox purchases** on the build.

## Meta Quest

This section covers the Meta Horizon Store: add-ons (consumables and durables) and subscriptions of a Quest app. The server looks them up with the Platform SDK's [server-to-server APIs](https://developers.meta.com/horizon/documentation/unity/ps-iap-s2s/) on `graph.oculus.com`.

### The build

Add a build with platform **Android** and store **Meta Horizon Store** (wizard → Add a build). Its app id only has to be unique among Meta builds: the package name, the same as the Google Play build's, is fine (a build is its store and its id).

Products use the add-on SKUs of the Developer Dashboard. Meta's server APIs report no price for add-ons. Give products a price, or let the client send the localized one (the Unity package does). A subscription period counts the price of its current term.

### Keys

1. In the Developer Dashboard, open the app → **Development → API**.
2. Copy the **App ID** into `meta_app_id` and the **App secret** into `meta_app_secret`.
3. In **Data Use Checkup**, request **User ID** and **In-App Purchases**, and **Subscriptions** if the app sells them.

The server calls Meta with the app access token `OC|<App ID>|<App secret>`. The setup check lists the app's add-ons with it.

!!! warning
    Without User ID, the client's user id is 0 and nothing can be verified.

### How purchases are checked

The client sends the player's Meta user id (`store_user_id`) and a user proof (`store_user_proof`). The proof is a nonce from `Users.GetUserProof`, good for one check. The server checks the proof with `user_nonce_validate` first, so nobody can claim another Meta user's purchases. Then it looks the purchase up:

- **Add-ons**: `viewer_purchases` lists what the user owns now (durables and consumables not consumed yet). `order_id` (the SDK's `Purchase.ID`) picks one, otherwise the latest of the SKU is used. The ledger id is `meta:<purchase id>`.
- **Subscriptions**: `application/subscriptions` (`owner_id`, the SKU) gives the current period. Each period is a ledger row of its own (`meta:<subscription id>.<period start>`). For a subscription with several terms, the product id may be the term's SKU (`<SKU>:SUBSCRIPTION__MONTHLY`). The server then asks for `<SKU>`.

The server consumes consumables (`consume_entitlement`) after it delivered the reward. When the client grants (`delivery: none`), the client consumes them after the server's answer.

!!! warning
    The client must not consume a purchase before the server answered. Meta no longer lists a consumed purchase, so the server could not verify it.

### No notifications

Meta sends no server notifications. Subscriptions are re-checked by the worker when their period ends (renewed, ended or extended), and refunds are not noticed.

Meta's answers do not tell test purchases apart either. Purchases of test users count as production.

### Client

Unity IAP has no Meta store. The Unity package uses `MetaQuestStoreAdapter` over a bridge to the Meta XR Platform SDK (sample "Meta Quest").

Other clients post these to `/verify/meta`:

- `store_user_id`;
- a fresh `store_user_proof` per request;
- the SKU as `product_id`.

They also send `store_user_proof` once per `/restore` call.

## Telegram Stars

Games in Telegram (Mini Apps, bots) sell digital goods for Telegram Stars (currency `XTR`). The server creates invoice links of the game's bot and acts as the bot's webhook. It answers the pre-checkout query, grants on `successful_payment` and revokes on `refunded_payment`.

### The build and its products

Add a build with platform **Web** and store **Telegram Stars**.

- **Prices** are in Stars: `price_micros` = Stars × 1,000,000 with currency `XTR` (50 Stars = `50000000`). A product without such a price cannot be sold.
- **Subscriptions** renew every 30 days (the only period Telegram offers) and cost at most 10,000 Stars.
- **The invoice** shows the product's title and description (the Products tab), else its catalog id.

### Keys and webhook

1. Store `telegram_bot_token`: the bot's token from @BotFather.
2. Generate `telegram_webhook_secret` on the Keys tab. Telegram sends it back in `X-Telegram-Bot-Api-Secret-Token`.
3. On the Setup tab, press **Set the bot webhook** (`admin_setup_telegram_webhook`). The bot's webhook becomes `…/notify/telegram?app=<app id>` with the secret, for `message` and `pre_checkout_query` updates.

!!! note
    A bot has one webhook. If your bot already has a backend, forward its `pre_checkout_query` updates and messages with `successful_payment` / `refunded_payment` to that URL with the header instead. The setup check then says the webhook points elsewhere.

The setup check reads the bot (`getMe`) and its webhook (`getWebhookInfo`): where it points and Telegram's last delivery error.

### Purchase flow

1. The client calls `POST /checkout/telegram` `{app_id, product_id, user_id}` → `{url, order_id}`. It opens `url` with `Telegram.WebApp.openInvoice`.
2. Telegram sends the pre-checkout query. The server accepts it when the order is known and the price unchanged. It answers in the webhook's response, within Telegram's 10 seconds.
3. `successful_payment` records the purchase for the order's player (`telegram:<telegram_payment_charge_id>`) and delivers it. The Telegram user id is kept for refunds. A subscription's renewals are new payments of the same order.
4. The client asks `POST /checkout/telegram/status` `{app_id, user_id, order_id}` (the shape of `/verify`) until it says `success`. In Unity, this client is `TelegramStoreAdapter` with the sample "Telegram Mini App".

Unpaid orders are forgotten after 7 days, and their links stop working. A subscription that was not paid again ends when its period is over. The worker re-checks it then.

### Refunds and revenue

Refund Stars with the Bot API `refundStarPayment` (your bot's support flow). The `refunded_payment` update revokes the purchase.

Revenue values a Star at $0.013, what Telegram pays developers, with no store fee on top. An exchange rate override for `XTR` (Revenue → Exchange rates) changes that.

## Xsolla

[Xsolla](https://developers.xsolla.com/) sells on the web and on desktop (Pay Station / Web Shop). It also sells in mobile games next to the app store, for example to players Google Play cannot charge.

There is no client receipt. Instead:

1. The server creates a payment token for one item.
2. The player pays in Pay Station.
3. Xsolla tells the server with [webhooks](https://developers.xsolla.com/webhooks/overview/).

Players never see the API key or the secret key.

### The build

Add a build with store **Xsolla** and pick its platform:

- **Web** for a web shop;
- **Android** or **iOS** for a mobile game that also sells through Pay Station. The attribution integrations then get the device ids, as for the app store build.

Its app id only has to be unique on the server, e.g. `com.example.mygame.web` or `com.example.mygame.xsolla`. Use the same value for the `xsolla` build in the Unity `BillingConfig`. Give it the same app as the app store build, so the customer info covers both.

Use **one Xsolla project per build**: a project has one webhook URL, and it names the build.

Products use the **item SKUs** of the project's catalog (Publisher Account → the project → **Items catalog**, virtual items or currency packages). The SKU is the product id. Kinds:

- a consumable for currency packages and consumable items;
- a non-consumable for items bought once.

Prices come from Xsolla's webhooks, so no default price is needed.

### Keys

Store these on the build (Keys tab):

- `xsolla_project_id`: the project ID shown in Publisher Account. It is a number and not secret, but it is kept with the keys so it never leaves the server.
- `xsolla_api_key`: Publisher Account → **Company settings → API keys**, or the project's **Project settings → API keys**. A company key works for every project of the company: store it on the developer account. The server uses the key with the project id (Basic auth `project_id:api_key`) to create payment tokens.
- `xsolla_webhook_secret`: the project's secret key, **Project settings → Webhooks → Secret key**.

The setup check lists the project's virtual items with the key (one call). It fails when Xsolla rejects the project id / key. It warns about products that are not enabled virtual items. Bundles and currency packages are not listed by that call.

### Webhooks

1. In Publisher Account, open the project → **Project settings → Webhooks**. Turn webhooks on and set the URL from the app's Setup tab: `https://<host>/v1/database/<db>/route/notify/xsolla?app=<app id>`.
2. Copy the **secret key** into `xsolla_webhook_secret`.
3. Send a test webhook from the page. A `user_validation` test answers `204` and the setup check shows *Last notification*.

A test payment for a SKU that is not a product of the build answers `500`. This way Xsolla retries real payments of products added later. A wrong secret shows up as *notifications are rejected (invalid signature)*.

Xsolla has two webhook models, and the server understands both:

- **Combined** (projects created since January 2025): the **`order_paid` / `order_canceled`** webhooks carry the payment data.
- **Separate** (older projects): **`payment` / `refund`** are sent, with `order_paid` / `order_canceled` alongside. Keep the `payment` webhook on: an `order_paid` without payment data is acknowledged and waits for it.

Purchases are keyed by the Xsolla transaction id, so a payment reported twice is granted once.

**User validation.** Xsolla may ask whether a user exists (`user_validation`). The server has no user list. It accepts any well-formed billing user id (letters, digits, `-_.:`, up to 128 characters) and answers `400 INVALID_USER` otherwise. The checkout puts the player's billing user id into the token, so Xsolla only ever sees ids the game chose.

**Refunds.**

- `refund` and `order_canceled` revoke the purchase in the game and count as refunds in revenue.
- A `partial_refund` leaves the purchase granted and takes the refunded amount (`purchase.total`) out of revenue. Each webhook is counted as one refund. A repeated delivery of the same refund (same transaction, refund date and amount) counts once.
- A refund in another currency than the payment is logged and not counted.
- `dispute` (chargeback) webhooks are acknowledged without a change. If Xsolla does not send a refund for a lost chargeback, revoke the purchase from the player's page in the admin panel.

**Subscriptions** (best effort). `/checkout/xsolla` sells catalog items only. Xsolla subscriptions use their own token with a plan, which this server does not create. A subscription sold another way is still recorded from its webhooks when the product id is the plan's external id:

- Every `payment` with subscription data grants it until `date_next_charge`.
- `cancel_subscription` ends it at `date_end`. If that date has passed when the webhook arrives, the subscription is revoked in the game. For a later date, the subscription simply ends then, without a revoke event.
- `create_subscription`, `update_subscription` and `non_renewal_subscription` need no action.

### Checkout

1. The client asks `POST /checkout/xsolla` with the app id, product id (SKU) and user id (optionally `sandbox`, `return_url`, `country`, `language`) and gets `{token, order_id, url}`.
2. It opens `url` (Pay Station) in a browser tab or window.
3. Xsolla calls the webhook. The server grants the purchase (delivery to the game as for every store).
4. The client polls `POST /checkout/xsolla/status` with the `order_id` every few seconds until `success`. Or it relies on its game server receiving the grant.

Xsolla needs the player's country to pick the currency. Pass `country` (ISO 3166-1 alpha-2). The player can change it in Pay Station. Without `country`, the server forwards the address from `X-Forwarded-For` when the hosting adds one.

**From a website or launcher without Unity.** The same three calls work from any client. Your page:

1. calls `/checkout/xsolla`, from your own backend or directly from the browser (only public ids travel);
2. redirects to `url` or opens it in a new window;
3. either polls `/checkout/xsolla/status` or waits for `return_url`.

Entitlements of non-consumables and subscriptions are in `POST /entitlements`, shared with the other builds of the game.

**Unity.** `XsollaStoreAdapter` does steps 1–4 (see the package README).

### In a mobile game, next to Google Play

The Unity package routes some purchases to Xsolla and the rest to the app store with `CompositeStoreAdapter`. For example, by the currency Google Play shows:

```csharp
var store = new CompositeStoreAdapter(Billing.CreateDefaultStore(config),
    new CurrencyStoreRouter(BillingStores.Xsolla, "RUB"),
    new XsollaStoreAdapter(config, new XsollaOptions { ReturnUrl = "mygame://purchase" }));
```

Pay Station opens in Chrome Custom Tabs on Android and in Safari on iOS. Chrome Custom Tabs need `androidx.browser`, requested from the External Dependency Manager. Without it, Pay Station opens in the default browser.

`return_url` may be an **app link** of the game (`mygame://...`, registered as the app's deep link). The server then adds a redirect policy that sends the player back right away. The package checks the order as soon as the link opens the game.

The routes do not change: `/checkout/xsolla` and `/checkout/xsolla/status` with the Xsolla build's app id.

### Moving purchases of an old client-side integration

Some games used the Xsolla SDK on the client alone (a device-id login, items in the Xsolla inventory). They can move what players own to the server once:

1. In a version that still has the old login, the client sends its Xsolla Login token to `POST /claim/xsolla`.
2. The server reads the player's inventory from Xsolla with that token. Xsolla rejects a forged or expired one.
3. The server records the **non-consumables** of the build as owned by the billing user.

Claimed items are not sales: no revenue, no integration events.

- A repeated claim changes nothing.
- The same Xsolla user claimed by another billing user (a reinstall with a new id) moves the items as a restore does, within the app's transfer limits.
- Optional key `xsolla_login_project_id` (Publisher Account → Login → the project ID): tokens of another login project are refused.

Unity: `IBillingService.ClaimXsollaPurchasesAsync(xsollaToken)`.

### Testing

Enable **Sandbox purchases** on the build and buy with `sandbox: true` (Unity: `XsollaOptions.Sandbox`). Pay Station then opens on `sandbox-secure.xsolla.com` and accepts Xsolla's test cards (see "Testing" in the Pay Station docs).

Test payments carry `dry_run = 1` and count as sandbox purchases. A build without sandbox purchases ignores them.

## Steam

Steam in-game purchases are [microtransactions](https://partner.steamgames.com/doc/features/microtransactions/implementation) driven by the server. The game asks the server to start an order, Steam shows its overlay, and the server captures the money.

The publisher Web API key stays on the server. It must never ship with the game. Steam has no purchase notifications, so the server polls Steam's transaction report for refunds and chargebacks.

### The build

Add a build with platform **Steam**. Its app id is the numeric **Steam App ID** (e.g. `480`).

A second build of the same App ID, say a sandbox build next to the live one, can use any unique app id. Name the App ID with `admin_set_steam_build` (`{"app_id": "com.example.mygame.steam-test", "steam_app_id": "480"}`).

### Publisher Web API key

1. In Steamworks, open **Users & Permissions → Manage Groups** → the group that owns the App ID → **Create WebAPI Key**.
2. Copy it into `steam_publisher_key`. One key serves every app of the group, so store it on the account scope.

The key needs access to the App ID (microtransactions).

If the group locks keys to IP addresses, either remove the lock or add the addresses the billing server calls from. Otherwise Steam answers 403 and the setup check reports *the publisher key was rejected or is not allowed from this IP*.

The server sends the key in the `x-webapi-key` header of `https://partner.steam-api.com/ISteamMicroTxn/...` calls, never in a URL.

### Items and prices

Every product of the build is sold as one Steam item (quantity 1):

- **Item id**: Steam needs a 32-bit number per item. By default it is a stable hash of the product id. Set `item_id` to use your own (e.g. the ids your Steam store packages use).
- **Description**: shown in the Steam overlay, up to 128 characters. By default it is the product id. Set a readable one with `description`.
- **Price**: Steam charges in the player's wallet currency, reported by `GetUserInfo` at checkout. The server uses the item's price in that currency if you set one (`prices`, in hundredths of the currency unit for every currency: `{"USD": 199, "EUR": 179, "JPY": 30000}`). Otherwise it uses the product's catalog price (`price_micros` + `currency`), which Steam converts at the current rate and shows converted.

!!! warning
    Some currencies only accept whole units (Steam rejects e.g. a UAH price of 10.50). Give those an explicit price.

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

1. The game calls `POST /checkout/steam` with:
    - the product;
    - the player's user id;
    - SteamID64 (`ISteamUser::GetSteamID`);
    - language (`ISteamApps::GetCurrentGameLanguage`);
    - a Web API auth ticket (`ISteamUser::GetAuthTicketForWebApi("purser")`, hex).

    The server checks that the ticket belongs to the SteamID, calls `GetUserInfo` and `InitTxn` and returns the order id. Steam shows the overlay.
2. The game receives `MicroTxnAuthorizationResponse_t`. When the player authorized, it calls `POST /verify/steam` with the order id. The server calls `FinalizeTxn` and `QueryTxn`, records the purchase and delivers the reward like any other store.
3. If the game dies in between, the worker finds the authorized order through `QueryTxn` and finishes it. The player gets the reward through delivery (or `/entitlements` / restore for owned products). Orders the player never authorizes are given up after the last re-check: 10 min, 30 min, 2 h, 6 h, 12 h, 24 h (about 45 hours).

### Refunds and chargebacks

Every 5 minutes per build, the worker asks `GetReport` (type `GAMESALES`) for transactions changed since the last call. It revokes purchases that became `Refunded`, `Chargedback`, `RefundedSuspectedFraud` or `RefundedFriendlyFraud` (and partial refunds of the item). The setup check shows when the report was last read.

### Subscriptions

A `subscription` product is sold as a Steam-billed recurring agreement:

- `billingtype Steam`;
- `period`: Day / Week / Month / Year;
- `frequency`: periods between charges (default monthly);
- the renewal amount is the first charge.

The ledger keeps one row per agreement. It runs until the day after `nextpayment` from `GetUserAgreementInfo`, and the worker asks again after that day:

- A renewal moves the expiry on, and the game gets `grant` again.
- A canceled agreement ends when its paid period does.

Renewal payments are not added to revenue as separate transactions.

Steam allows one active agreement per player and game. A new agreement replaces the previous one.

### Auth tickets

Without a ticket, anyone who knows a player's SteamID could start orders in their name.

1. Make every build of the game send tickets. The Unity package does when the bridge implements `ISteamWebApiTicketProvider`, as the sample does.
2. Set `"require_ticket": true` with `admin_set_steam_build`. Checkouts without a valid ticket are then refused.

The setup check warns while `require_ticket` is off.

### Testing

Set `"sandbox": true` with `admin_set_steam_build` and enable **Sandbox purchases** on the build. Every call then goes to `ISteamMicroTxnSandbox`, no money moves, and purchases are recorded as sandbox.

!!! warning
    Turn it off for the release build. The setup check warns while it is on.

### Client

The Unity package has `SteamStoreAdapter`, with a Steamworks.NET bridge in the sample "Steam".

Other clients:

1. Call `/checkout/steam`.
2. Wait for `MicroTxnAuthorizationResponse_t` with the returned order id.
3. Call `/verify/steam`.

## Stripe

Web builds can sell through [Stripe Checkout](https://docs.stripe.com/payments/checkout). The server creates the Checkout Session and the player pays on Stripe's page. Stripe's [webhooks](https://docs.stripe.com/webhooks) tell the server about payments, renewals, cancellations, refunds and disputes.

There is no client receipt. The webhook is the source of truth. `/verify/stripe` only lets the client get its answer right after the redirect.

### The build and its products

1. Add a build with platform **Web** and store **Stripe** (wizard → Add a build). Its app id only has to be unique, e.g. `com.example.mygame.web`. It is the `stripe` build in the Unity `BillingConfig`. A game selling through Stripe and Xsolla has a build for each.
2. In the Stripe Dashboard, open **Product catalog** and create a product per item with a **Price**: one-time for consumables and non-consumables, recurring for subscriptions.
3. Add the products to the build with the **Price id** (`price_…`) as the product id and the matching kind.

!!! warning
    Test mode and live mode have different Price ids. Use a test build (sandbox purchases on) and a live build, or change the ids when going live.

Entitlements of a Stripe build name the Price ids. The Unity adapter maps your product ids to them (`StripeCheckoutOptions.PriceIds`).

The setup check fetches every product's Price (up to 8 per run) and reports missing, archived or wrong-type Prices.

### Keys

- `stripe_secret_key`: Developers → **API keys** → the secret key (`sk_live_…` / `sk_test_…`). There is one per Stripe account, so store it on the developer account. A [restricted key](https://docs.stripe.com/keys/restricted-api-keys) (`rk_…`) works too with **Checkout Sessions: Write** and **Prices, Subscriptions, Invoices: Read**. Add **Customer portal: Write** for the customer portal below.
- A test-mode key makes sandbox purchases. Enable **Sandbox purchases** on the build, or every purchase is ignored (the setup check fails then).

### Webhook endpoint

**From the admin page.** Once the key is stored, press **Create webhook in Stripe** next to the build on the app's **Setup** tab (`admin_setup_stripe_webhook`, needs the Secrets role). The server creates the endpoint with the events below and stores its signing secret.

- An endpoint that exists already gets the missing events.
- If its secret is not stored here, the page offers to replace it. Stripe shows a secret only when it creates an endpoint.
- A restricted key needs **Webhook Endpoints: Write** for this.

The setup check shows whether the endpoint exists and receives every event.

By hand:

1. Open Developers → **Webhooks** → **Add destination**. Choose events from **your account**, payload style **Snapshot**, any API version. Set the endpoint URL from the app's Setup tab: `https://<host>/v1/database/<db>/route/notify/stripe?app=<app id>` (one endpoint per build).
2. Select these events:
    - `checkout.session.completed`, `checkout.session.async_payment_succeeded`
    - `invoice.paid` (or `invoice.payment_succeeded`)
    - `customer.subscription.updated`, `customer.subscription.deleted`
    - `charge.refunded`, `charge.dispute.created`, `charge.dispute.closed`
3. Copy the endpoint's **Signing secret** (`whsec_…`) into `stripe_webhook_secret` of the build.

How the server handles the webhooks:

- Every webhook is checked with its `Stripe-Signature` (5 minutes tolerance). A wrong secret shows up in the setup check as *notifications are rejected*.
- Stripe retries failed deliveries for three days. Retries and duplicates never grant twice.
- Partial refunds keep the purchase and take the refunded part out of revenue.
- A full refund or a dispute revokes the purchase. A dispute you win grants it again (reason `reinstated`).
- Plan changes (customer portal, API) are followed. The invoice that charges the new Price makes it the subscription's product and ends the old one (reason `replaced`). Every Price you let players switch to must be a product of the build.

### Customer portal

Players cancel a subscription, change the card and download invoices in Stripe's [customer portal](https://docs.stripe.com/customer-management).

Set it up once per mode: Stripe Dashboard → **Settings → Billing → Customer portal** → choose what customers may do → **Save**. Live mode refuses portal sessions until it is saved. If you let players switch plans, list only Prices that are products of the build.

- **Builds with player tokens** (`client_auth = signed`): `POST /manage/stripe` `{"app_id", "user_id", "return_url"}` → `{"status": "ok", "url"}` creates a portal session for the Stripe customer of the player's active subscription (two Stripe calls). The Unity package opens it from `OpenManageSubscriptionsAsync()`. A player without a subscription gets `404 ERR_NO_SUBSCRIPTION`.
- **Builds without player tokens** get `403 ERR_PLAYER_TOKENS_REQUIRED`. The portal acts for the customer, and without tokens anyone who knows a player id could open it. Instead, turn on the portal's **Login link** on the same Dashboard page and use it (`https://billing.stripe.com/p/login/…`). The customer signs in with the email of their payments. In Unity, set `StripeCheckoutOptions.CustomerPortalLoginUrl`. Elsewhere, put a link on your page.

### Test

Use a `sk_test_` key, Sandbox purchases on, and a test card (`4242 4242 4242 4242`). The ledger row is a `sandbox` purchase with the Stripe transaction id:

- `stripe:pi_…` for one-time purchases;
- `stripe:sub_…` for the first subscription period;
- `stripe:in_…` for renewals.

[`stripe trigger`](https://docs.stripe.com/cli/trigger) events have none of the server's metadata and are ignored. Test through `/checkout/stripe`.

### Revenue

Stripe reports the price paid, in the currency the customer paid. Zero-decimal currencies like JPY are handled.

Stripe's processing fee is not a store commission. Set the proceeds estimate for Stripe builds with the per-build commission (Revenue → Store fee in the admin page). For example, `admin_set_setting("store_commission_by_app", "{\"stripe:com.example.mygame\": 3}")` (keys are build keys, `{store}:{app id}`) (≈ card fee) or `0`.

Taxes collected by Stripe Tax are part of the amount.

### Client

- **Unity**: `StripeStoreAdapter` (see the package README) does the steps below. It starts the checkout, opens the page and polls `/verify/stripe` until the session is granted.
- **Any website** (no Unity), with the player's id known to the page:
    1. Call `POST /checkout/stripe` `{"app_id", "product_id": "price_…", "user_id", "success_url": "https://game.example/thanks?session={CHECKOUT_SESSION_ID}", "cancel_url"}` and redirect to `url`.
    2. On the success page, call `POST /verify/stripe` `{"app_id", "user_id", "session_id"}`. It returns `success` once paid, or `pending_or_canceled` while not: ask again in a few seconds. You can also read `/entitlements`.
    3. The game backend receives `grant` / `revoke` as for any store. With `delivery = none`, the page grants on `success`.

## Creating products from the admin page

**Create in store** on an app's Products tab creates the app's products in Google Play, the App Store, Stripe and Xsolla. You pick:

- the products and the builds to create them in;
- an English title and description;
- the price and currency;
- for subscriptions, the billing period.

The title, description, period and App Store subscription group start from the product's **store listing**. You edit the listing in the product dialog, and it is saved again after each product is created. So they are typed once for every store and build.

**Check** shows, for every build and product, whether it will be created. A product the store already has is skipped and can be added to the build's products instead.

Each product then runs as a job in the background. The list under the products shows its progress and errors. A finished product is added to the build's products.

The store converts the price to the other countries:

- **Google Play.**
    - Prices: Google converts them to every region.
    - Activation: one-time products get an active purchase option `default`. Subscriptions get an active auto-renewing base plan named after the period (e.g. `p1m`).
    - Permissions: the service account needs **Manage store presence**.
    - Before you start: the app needs a build with Play Billing uploaded to any track.
- **App Store.**
    - Key: needs the [App Store Connect API Team key](#catalog-import-optional) (App Manager or Admin role).
    - Prices: the base price is the nearest App Store price point in the territory of the currency (USD → United States, EUR → Germany, …). The job notes when it differs. Apple fills in the other territories for in-app purchases. For subscriptions, the job sets every territory's equalized price, which takes about 180 calls.
    - Subscriptions go to the subscription group you name, which is created if missing.
    - Left for App Store Connect: the review screenshot and the review submission. The first in-app purchase of an app must be submitted with an app version.
- **Stripe.**
    - Creates a Product with a default Price (recurring for subscriptions). This is one request, with an idempotency key.
    - The new **Price id** becomes the build's product id. Put it into the Unity config's store ids.
    - Permissions: a restricted key needs Products: Write and Prices: Write.
- **Xsolla.**
    - Creates an enabled virtual item with the SKU = product id and one price.
    - Subscriptions are not supported.
- **Amazon Appstore and Steam:** not supported. Amazon has no API for in-app items. Use the Developer Console or its CSV upload. Steam items are defined by the game itself.

## Sandbox and testers

Enable **Sandbox purchases** for the builds on the app's Delivery tab for App Review, TestFlight and license testers. Sandbox purchases are excluded:

- from revenue by default (Revenue → *Include sandbox*);
- from integrations unless their *Send sandbox / test purchases* option is on.
