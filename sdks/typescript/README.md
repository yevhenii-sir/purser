# @purser/client

TypeScript client of a self-hosted [Purser](../../README.md) server for web games, Cocos Creator, PlayCanvas and
Telegram Mini Apps. No runtime dependencies: it uses `fetch` and Web Crypto. It speaks the same HTTP contract as the
Unity package (`server/README.md`, "HTTP API").

## Install

```bash
npm install @purser/client
# before it is on npm: from a checkout of the repository
npm install ../purser/sdks/typescript      # or `npm pack` there and install the .tgz
```

ES module, `ES2020`; runs in browsers, Node 20+, Cocos Creator 3.x and PlayCanvas scripts (bundle it with your game).

## Setup

```ts
import { PurserClient } from "@purser/client";

const purser = new PurserClient({
  baseUrl: "https://stdb.example.com/v1/database/<your-billing-db>/route",
  appId: "com.example.mygame.web",          // the build's app_id
  userId: player.id,                         // or purser.setUserId(...) after sign-in
  client: { app_version: "1.2.1" },          // device and os default to what the browser reports
  // Builds with client_auth = signed: a player token from your backend (HS256 JWT, see docs/game-integration.md).
  getPlayerToken: (userId) => fetch(`/api/purser-token?user=${userId}`).then((r) => r.text()),
  // Builds with a response signing key: the public key from the app's Delivery tab.
  responsePublicKey: "<base64 Ed25519 public key>",
});
```

Every method resolves with `{ httpStatus, value?, error?, code?, retryAfterMs? }` and never rejects for HTTP or network
failures. Requests are retried after a transport error, a 5xx or a 429 (waiting its `Retry-After`, at most 60 s), never
after another 4xx (`retries`, `retryDelayMs`, `timeoutMs` change it). A `401 ERR_CLIENT_AUTH` asks `getPlayerToken` once
more and sends the request again once. With `responsePublicKey`, `/entitlements` and `/restore` answers without a valid
`X-Purser-Signature` are errors (`value` is undefined).

## Purchases

The flow is always: the store purchase, then `verify`, then the store's finish step only after the server's answer.
`processPurchase` applies the Unity package's rules:

| Server answer | Outcome | Store purchase |
|---|---|---|
| `success` (200), `pending` (202) | `success`, `pending_delivery` | `grant(answer)` first when `delivery` is `client`, then `finish(true)` |
| `invalid`, `forbidden`, `expired`, `revoked` | `rejected` | `finish(false)`: Apple `finishTransaction`; leave a **Google** purchase unacknowledged (Google refunds it in 3 days) |
| `pending_or_canceled` | `pending_payment` | keep it; the store reports it again when paid |
| no network, 429, 5xx, `error` (after the retries) | `retry` | keep it; verify it again later (next launch, the store re-delivers it) |

```ts
const done = await purser.processPurchase("google", {
  productId: "gems_100",
  purchaseToken: purchase.purchaseToken,     // Google; Apple and Amazon: orderId (+ storeUserId for Amazon)
  orderId: purchase.orderId,
  priceMicros: 990000, currency: "USD",      // the localized price the store showed
  paywall: shownPaywall,                     // a /paywall answer, for A/B test results
}, {
  grant: (answer) => inventory.add("gems", 100 * (answer.quantity ?? 1)),   // only for delivery = client builds
  finish: (granted) => granted ? plugin.finish(purchase) : plugin.finishRefused(purchase),
});
```

A throwing `grant` leaves the purchase unfinished: the store delivers it again, the server answers `success` from its
ledger, and `transaction_id` tells whether it was granted already. Bind purchases to the player before buying:
`googleAccountId(userId)` (Google `obfuscatedAccountId`) and `appleAccountToken(userId)` (StoreKit `appAccountToken`);
the server refuses consumables bound to another player.

### Stripe (web)

```ts
const checkout = await purser.stripeCheckout("price_123", location.origin + "/shop?session={CHECKOUT_SESSION_ID}", location.href);
if (checkout.value?.url) location.href = checkout.value.url;

// back on /shop?session=cs_...
const sessionId = new URLSearchParams(location.search).get("session");
if (sessionId) {
  await purser.applyOutcome(await purser.verifyStripe(sessionId), {
    grant: (a) => grantLocally(a),            // delivery = client builds
    finish: () => {},                         // nothing to finish in Stripe
  });
}
const portal = await purser.manageStripe(location.href);   // the player's subscription (builds with player tokens)
```

### Telegram Stars (Mini Apps)

```ts
const checkout = await purser.telegramCheckout("gems_100");
Telegram.WebApp.openInvoice(checkout.value!.url, async (status) => {
  if (status !== "paid") return;
  // The bot's webhook records the payment; poll until the server has it.
  const result = await purser.waitForTelegramPayment(checkout.value!.order_id!);
  await purser.applyOutcome(result, { grant: (a) => grantLocally(a), finish: () => {} });
});
```

### Cocos Creator, PlayCanvas, other native wrappers

Whatever IAP plugin the game uses on Android / iOS, hand its purchase to `processPurchase` with the store's route
(`google`, `apple`, `amazon`, `huawei`) and call the plugin's consume / acknowledge / `finishTransaction` from `finish`.
Turn off the plugin's automatic finishing.

## Restore and entitlements

```ts
const restored = await purser.restore(ownedPurchases.map((p) => ({ productId: p.productId, purchaseToken: p.token })));
const info = (await purser.entitlements()).value;
if (PurserClient.hasAccess(info, "premium")) unlockPremium();
```

## Paywalls, attributes, promo codes, wallet

```ts
const paywall = (await purser.paywall("main_paywall")).value;      // products, remote_config, A/B variant
await purser.paywallEvent(paywall!);                                // the paywall was shown
await purser.setAttributes({ level: 7, clan: "wolves", old_key: null });
const redeemed = await purser.redeem("SUMMER-2026");               // the shape of /verify; code ERR_CODE_* on 404 / 409
const balances = (await purser.wallet()).value?.balances;           // { gems: 120 }
const spent = await purser.spend("gems", 30, "sword-42");           // builds with player tokens; retry with the same reference
```

Other client routes: `purser.post(path, body)`.

## Tests

```bash
npm install
npm test        # tsc + node --test (Node 22+), a fake fetch, no network
UPDATE_SDK_REQUESTS=1 npm test   # rewrites server/spacetimedb/sdk-requests/typescript.json
```

`server/spacetimedb/sdk-requests/typescript.json` holds what this SDK sends; the module's tests parse every body.
