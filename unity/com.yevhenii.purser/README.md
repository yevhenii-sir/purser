# Purser for Unity

In-app purchases in Unity on top of Unity IAP 5. Every purchase is verified by your Purser server on SpacetimeDB (see `server/` in this repository). The server:

- verifies the purchase with Google Play or the App Store;
- consumes/acknowledges it;
- delivers the reward into the game (a SpacetimeDB reducer or a webhook);
- serves entitlements — what the player has bought.

All the client has to do is show the store and react to the result.

- Unity 2022.3+, `com.unity.purchasing` 5.4.4+, Newtonsoft JSON.
- The core is not tied to any DI: the `IBillingService` interface and the `BillingService` class. There's a ready-made `BillingBootstrap` component and optional adapters for VContainer and Zenject.

## Installation

`Packages/manifest.json`:

```json
"com.yevhenii.purser": "https://github.com/yevhenii-sir/purser.git?path=/unity/com.yevhenii.purser"
```

To pin a specific version, append `#<tag or commit>` to the URL.

## Setup

1. **Tools → Purser → Create Config**.
2. Fill in the config in either direction:
   - **Unity first** (the game already has its products): fill in the server address
     `https://maincloud.spacetimedb.com/v1/database/<your-billing-db>/route`, the builds and the products, press
     **Copy config for admin panel** and paste it in the admin panel (**Apps → Add an app → Paste from Unity**, or an
     app's **Products → Import from Unity**). The builds and products are created there.
   - **Admin panel first**: copy the config from the app's **Products → Copy Unity config** (or the last step of
     **Add an app**) and press **Paste config from admin panel** in the config inspector. A window lists what changes
     before anything is applied, in one of two modes: **Add and update** (new builds and products are added, matching
     ones updated, everything only Unity has stays) or **Match the admin panel** (also removes builds, products and store
     ids the admin panel does not have). Applying is one undo step. In code: `PreviewJson` / `MergeJson(json, mode)`.

   The fields:
   - `builds` — one row per store the game sells in: the store (`google`, `apple`, `amazon`, `steam`, `stripe`,
     `xsolla`) and the build's app id on the server (Android package, iOS bundle id, or any unique id such as
     `com.example.mygame.web`). A store without a row uses `Application.identifier`. A web game can have both a
     `stripe` and an `xsolla` build: each adapter sends its own app id.
   - `products` — `id` (what the game code uses), `type` (Consumable / NonConsumable / Subscription),
     `storeIds`: rows for the stores where the product has another id (e.g. a reverse-domain App Store id), and an
     optional `fallbackPrice` + `fallbackCurrency`.
   - The fallback price is shown when the store gives none (Steam, Stripe, Xsolla, the Editor): `StoreProduct.Price`,
     `Currency` and `LocalizedPrice` (`0.99 USD`) are filled from it and `IsFallbackPrice` is set. It is never reported
     to the server as revenue. Copied into the admin panel, it becomes the default price of products that have none
     there; pasted back from the admin panel, the server's default prices fill it.
3. The **Validate** and **Ping server** buttons in the config inspector check the settings and whether the server is
   reachable; **Open admin panel** opens the server's admin page (`{serverBaseUrl}/admin`).

## Usage

### Without DI

**Tools → Purser → Add Bootstrap To Scene**, assign the config. Then in code:

```csharp
var billing = BillingBootstrap.Current.Service;
billing.SetUserId(playerId);              // after login: the player's identity hex / id in your backend

billing.PurchaseSucceeded += r => ShowReward(r.ProductId);    // also fires for purchases completed in the background
var result = await billing.PurchaseAsync("gems_100");
if (result.IsSuccess) { /* the server accepted it, the reward is in the game or on its way */ }

bool vip = billing.IsActive("vip");                          // non-consumable or active subscription
DateTime? until = billing.GetExpiration("premium");
await billing.RestoreAsync();                                // "Restore purchases" button (required on iOS)
```

### Manual construction

```csharp
IBillingService billing = Billing.Create(config, new FuncUserIdProvider(() => conn.Identity?.ToString()));
await billing.InitializeAsync();
```

### Games without a backend (local grant)

If the game has no backend, set `delivery = none` for the app in billing. Billing still verifies the purchase with the store, keeps the ledger and sends analytics, but the game grants the reward itself. If there's no login either, use `LocalUserIdProvider`: an anonymous GUID created on first launch and saved in PlayerPrefs.

```csharp
IBillingService billing = Billing.Create(config, new LocalUserIdProvider());

billing.PurchaseSucceeded += r =>
{
    if (!r.GrantOnClient) return;                         // the game server granted the reward
    if (PlayerPrefs.HasKey("iap." + r.TransactionId)) return; // already granted
    AddGems(r.ProductId);
    PlayerPrefs.SetInt("iap." + r.TransactionId, 1);
    PlayerPrefs.Save();                                   // save before leaving the handler
};

bool noAds = billing.IsActive("no_ads");                  // non-consumables and subscriptions — via entitlements
```

How this protects the reward:
- The server responds with `delivery: "client"` (`PurchaseResult.GrantOnClient`). For Google consumables it only acknowledges, so Google won't refund after 3 days, and leaves the consume to the client.
- The package fires `PurchaseSucceeded` first and finishes the purchase in the store only **after** the handlers: it consumes in Google or calls `finishTransaction` in Apple. If the game crashes before that, the store delivers the purchase again, the server answers `success` again, and the handler uses `TransactionId` to tell whether the reward was already granted.
- Grant the reward in `PurchaseSucceeded` and save it there too. The continuation of `await PurchaseAsync(...)` may run after the purchase has already been finished in the store.
- If a handler throws, the purchase isn't finished and the store delivers it again.

After a reinstall the id will be new: non-consumables and subscriptions come back via `RestoreAsync()` (on Android the store returns them on startup anyway), and consumables don't need restoring.

### VContainer

The adapter is enabled automatically when `jp.hadashikick.vcontainer` is installed:

```csharp
builder.RegisterPurser(config);
```

If `IUserIdProvider` and `IAnalyticsIdProvider` are registered in the container, the adapter picks them up.

### Zenject / Extenject

Add the `PURSER_ZENJECT` scripting define and put `PurserInstaller` on a context (or call `PurserInstaller.Install(Container, config)`).

## How a purchase is processed

The store reports an order: a new purchase, a redelivery after restart, or a restore. The order is sent to `POST /verify/{google|apple}`.

| Server response | What the package does | `PurchaseStatus` |
|---|---|---|
| `success` | finish in the store, `PurchaseSucceeded` event; with `delivery: "client"` — the event first, then finish | `Success` |
| `pending` (202) | finish in the store, the server will deliver the reward | `PendingDelivery` |
| `pending_or_canceled` | doesn't confirm, the store will deliver the order again | `PendingPayment` |
| `invalid` / `forbidden` / `expired` / `revoked` | finish in the store as not granted (so the order doesn't keep coming back; Amazon: `UNAVAILABLE`), `PurchaseFailed` | `Rejected` |
| `error`, 5xx, no network | doesn't confirm, retries after `retryDelaysSeconds`, then on the next launch | `PendingVerification` |

Details:
- If the `user_id` isn't known yet (before login), the order waits until a player appears.
- The server consumes Google consumables, so the store's “already consumed” message after `ConfirmPurchase` is expected; the package only writes it to the verbose log. The exception is local grant (`delivery = none`): there `ConfirmPurchase` consumes after `PurchaseSucceeded`.
- In the Editor by default (`editorMode = SkipServer`) purchases from the Unity IAP fake store are confirmed locally, without the server, which is convenient for debugging UI.

## Paywalls, remote config and A/B tests

Ask the server what to show at a placement; products, their order and the remote config change in the
admin page (app → Paywalls) without an app update, and A/B tests split players between paywalls.

```csharp
try
{
    var paywall = await billing.GetPaywallAsync("main_paywall");   // cached for offline starts
    title.text = paywall.GetValue("title", "Go premium");           // remote config (also RemoteConfigJson, GetRemoteConfig<T>())
    foreach (var p in paywall.Products)                              // BillingConfig products in the server's order
        AddButton(p.LocalizedPrice, () => billing.PurchaseAsync(p.Id, paywall));  // the purchase counts for the variant
    _ = billing.LogPaywallShownAsync(paywall);                       // one view per display
}
catch (PaywallNotFoundException)
{
    ShowBuiltInPaywall();
}
```

Products must be in `BillingConfig` (Unity IAP fetches only configured products); unknown ones are listed
in `Paywall.MissingProductIds`. Details: [docs/paywalls.md](https://github.com/yevhenii-sir/purser/blob/main/docs/paywalls.md).

## Amazon Appstore, Steam, web

Unity IAP 5 covers Google Play and the App Store. Other stores plug in through `BillingDependencies.Store`:

- **Amazon Appstore**: `AmazonStoreAdapter` over a bridge to Amazon's Appstore SDK Unity plugin (import the
  **Amazon Appstore** sample, it has the bridge). Add an `amazon` build with the app id of the Amazon build on
  the server and, if the SKUs differ, `amazon` rows in the products' `storeIds`. `androidStore = Auto` treats an install by the
  Amazon Appstore as an Amazon build. The server verifies the receipt id with the Amazon user id
  (`/verify/amazon`); the adapter calls `NotifyFulfillment` after the server's answer (`UNAVAILABLE` when it
  rejected the receipt).
- **Xsolla** (WebGL, desktop): `XsollaStoreAdapter`. Add an `xsolla` build with the app id of the Xsolla build; the
  product id is the Xsolla item SKU. A purchase asks the server for a payment token (`/checkout/xsolla`), opens
  Pay Station with `Application.OpenURL` and polls `/checkout/xsolla/status` until Xsolla's webhook reached the
  server; then `PurchaseSucceeded` fires as for any store (with `GrantOnClient` for builds without a backend).
  A payment not finished within `XsollaOptions.PaymentTimeout` (10 minutes) is reported as `PendingPayment`;
  the order is remembered and checked again at the next launch, or now with `ResumePendingOrders()`.

  ```csharp
  var xsolla = new XsollaStoreAdapter(config, new XsollaOptions
  {
      ReturnUrl = "https://example.com/mygame",   // where Pay Station sends the player back
      Country = "DE",                            // optional: picks the currency (the player can change it)
      Sandbox = Debug.isDebugBuild,              // test cards; the build must allow sandbox purchases
  });
  var billing = new BillingService(config, new BillingDependencies { Store = xsolla });
  ```

  On WebGL a tab opened after an `await` may be blocked as a popup: pass `XsollaOptions.OpenUrl` that opens the
  page from a `.jslib` (same tab, or a window opened in the click handler). Prices are shown by Pay Station
  (`StoreProduct.LocalizedPrice` is empty), and subscriptions are not sold through the adapter. The checkout
  carries the attribution ids (`IAttributionProvider`). Server side and use without Unity:
  [docs/store-setup.md#xsolla](https://github.com/yevhenii-sir/purser/blob/main/docs/store-setup.md#xsolla).
- **Stripe (web)**: `StripeStoreAdapter`. Add a `stripe` build with the app id of the Stripe build. A purchase asks the server for a Checkout Session
  (`/checkout/stripe`), opens its page (`StripeCheckoutOptions.OpenUrl`, default `Application.OpenURL`) and polls
  `/verify/stripe` with the session id until it is paid and granted; then `PurchaseSucceeded` fires as for any
  store. A payment not finished within `PaymentTimeout` (10 minutes) is reported as `PendingPayment`; the session
  is remembered and checked again at the next launch (or now with `ResumePendingOrders()`), so a same-tab
  redirect to Stripe works on WebGL, where a tab opened after an `await` is often blocked as a popup.
  The session check carries the attribution ids and the paywall of the purchase (A/B test results).
  Product ids of the Stripe build on the server are Stripe Price ids: map yours with `PriceIds` or `stripe` rows in
  the products' `storeIds` (entitlements of the Stripe build name the Price ids).

  ```csharp
  // Assets/Plugins/WebGL/StripeRedirect.jslib:
  //   mergeInto(LibraryManager.library, { StripeRedirect: function (url) { window.location.href = UTF8ToString(url); } });
  [System.Runtime.InteropServices.DllImport("__Internal")] static extern void StripeRedirect(string url);

  var stripe = new StripeStoreAdapter(config, new StripeCheckoutOptions
  {
      SuccessUrl = "https://example.com/mygame/?stripe_session={CHECKOUT_SESSION_ID}",
      CancelUrl = "https://example.com/mygame/",
      PriceIds = { ["gems_100"] = "price_1Pxyz" },
      OpenUrl = StripeRedirect,                    // WebGL: same tab; elsewhere leave the default
  });
  var billing = new BillingService(config, new BillingDependencies { Store = stripe });
  // A session id from the success URL can also be checked directly: stripe.ResumeSession(id).
  ```

  Prices are not loaded from Stripe (`StoreProduct.Price` is 0): show them from your paywall's remote config.
  Server and Stripe Dashboard setup, and use without Unity:
  [docs/store-setup.md#stripe](https://github.com/yevhenii-sir/purser/blob/main/docs/store-setup.md#stripe).
- **Steam** (Standalone): `SteamStoreAdapter` over `ISteamMicroTxnBridge` (import the **Steam** sample: a bridge
  for Steamworks.NET behind `PURSER_STEAMWORKS_NET`). Add a `steam` build with the Steam App ID of the build on
  the server. A purchase asks the server to start the order (`/checkout/steam`; the server holds the publisher Web API
  key), Steam shows its overlay, and when `MicroTxnAuthorizationResponse_t` says the player authorized it the order
  goes through `/verify/steam` (the server finalizes it) and `PurchaseSucceeded` fires as for any store. Declining is
  `Cancelled`; no answer within `SteamOptions.AuthorizationTimeout` is `Failed`. Authorized orders the server has
  not answered yet are verified again at the next launch. Prices are set on the server and shown by the overlay
  (`StoreProduct.LocalizedPrice` is empty).

  ```csharp
  var steam = new SteamStoreAdapter(config, new SteamworksNetBridge(() => SteamManager.Initialized));
  var billing = new BillingService(config, new BillingDependencies { Store = steam });
  ```

  Server side: [docs/store-setup.md#steam](https://github.com/yevhenii-sir/purser/blob/main/docs/store-setup.md#steam).
- **Another store**: implement `IStoreAdapter`; its `Store` is the server's store id, which picks the route
  (`/verify/{store}`), the build in `builds` and the product ids in `storeIds`. A store whose purchase starts on the
  server derives from `ServerCheckoutStoreAdapter` (pending orders kept across launches, config products, polling);
  one that learns about payments by webhook also implements `IWebhookStoreAdapter`.

`BillingBootstrap` takes a component implementing `IStoreAdapterFactory` for these adapters; the VContainer and
Zenject registrations use a bound `IStoreAdapter`.

## Binding a purchase to the player

With `bindPurchasesToUser = true` the package passes to the store:
- Google: `obfuscatedAccountId` = `user_id` (if longer than 64 characters — SHA-256 hex);
- Apple: `appAccountToken` = a UUID derived from SHA-256(`user_id`).

For consumables the server computes the same value and refuses (`403 forbidden`) if someone tries to credit the purchase to a different player. Non-consumables and subscriptions follow the store account: “Restore purchases” with a new `user_id` (reinstall, new device) moves them to the new player. The rule is shared with the server (`AccountIds` ↔ `purser_core::google_account_id` / `apple_account_token`) and checked with the same test vectors on both sides.

## App Store promotional offers

Discounts for current or lapsed subscribers, set up in App Store Connect (Subscription Prices → Promotional Offers):

```csharp
var result = await billing.PurchaseWithPromotionalOfferAsync("premium", "winback_50");
```

The server signs the offer for the current player (`POST /offers/apple/sign`, with the In-App Purchase key it already has), and the package buys through Unity IAP with the offer and the player's `appAccountToken`; the purchase is then verified like any other. App Store only: on Google Play it throws `BillingException` (Google subscription offers are picked on the client by their offer token). In the Editor the product is bought without the offer. `GetApplePromotionalOfferAsync` returns the signed offer (also as a V2 JWS) for your own StoreKit code. Unity IAP 5 has no public API for offers, so the package passes them to Unity IAP's StoreKit 2 bridge through its internals and fails rather than buying at the full price if they change. Details: [docs/apple-offers.md](../../docs/apple-offers.md).

## GA4, AppsFlyer, Adjust, Tenjin, AppMetrica, Apple Ads

The server sends purchase events to the connected integrations itself. The client only has to pass device identifiers:

- **GA4:** implement `IAnalyticsIdProvider` (for example via `FirebaseAnalytics.GetAnalyticsInstanceIdAsync()`) so events are tied to the right `app_instance_id`.
- **AppsFlyer:** pass `appsflyer_id` via `IAttributionProvider`. AppsFlyer won't accept the event without it.
- **Tenjin:** pass `tenjin_id` (the Tenjin SDK's analytics installation id) and `os_version`, plus `advertising_id` / `idfa` and `idfv` when the player consented.
- **Adjust:** pass `adjust_id` (`Adjust.GetAdid()`), or at least `advertising_id` / `idfa` / `idfv`.
- **AppMetrica:** pass `appmetrica_device_id` (from `AppMetrica.RequestAppMetricaDeviceID`), or set the AppMetrica profile id to the billing user id and turn on "profile id" in the integration.
- **Apple Ads:** tick `collectAppleAdsAttribution` in `BillingConfig`. On iOS 14.3+ the SDK takes an AdServices token, asks Apple once per install which campaign brought it and adds `asa_campaign_id`, `asa_ad_group_id`, `asa_keyword_id`, … to every purchase. No framework setup: the native part loads AdServices at run time.

```csharp
var attribution = new FuncAttributionProvider(() => new Dictionary<string, string>
{
    [AttributionKeys.AppsFlyerId] = AppsFlyer.getAppsFlyerId(),
    // [AttributionKeys.AdvertisingId] = gaid,  // only with the player's consent
});
var billing = Billing.Create(config, users, attributionProvider: attribution);
```

`BillingBootstrap` has an `Attribution Provider` field for this: assign a component that implements `IAttributionProvider`. In DI containers, registering `IAttributionProvider` is enough.

## App Store refund requests

When a player asks Apple for a refund, the server can tell Apple whether the purchase was delivered and used (turn on "Refund requests" for the iOS build in the admin page). Apple only accepts this for players who agreed to share the data, so ask with your own prompt (not the ATT one) and record the answer:

```csharp
billing.SetRefundDataConsent(playerAgreed); // stored on the device, sent with the next purchase or restore
```

Details and what is sent: [docs/store-setup.md](../../docs/store-setup.md#refund-requests).

## Tests

Add the package to `"testables"` in the project's `manifest.json` and run the Test Runner (EditMode): the tests use a fake store and a fake server, no network needed.
