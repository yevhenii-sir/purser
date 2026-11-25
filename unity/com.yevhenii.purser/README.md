# Purser for Unity

In-app purchases in Unity on top of Unity IAP 5. Every purchase is verified by your Purser server on SpacetimeDB (see `server/` in this repository). The server:

- verifies the purchase with Google Play or the App Store;
- consumes/acknowledges it;
- delivers the reward into the game (a SpacetimeDB reducer or a webhook);
- serves the customer info — the access levels, subscriptions and non-consumables the player has.

All the client has to do is show the store and react to the result.

- Unity 2022.3+, Newtonsoft JSON. Google Play and the App Store through the package (the default store adapter)
  need Unity IAP (`com.unity.purchasing`) 5.4 or later: add it to the project yourself, the package does not depend on
  it. Without Unity IAP 5.4+ the adapter (assembly `Purser.UnityIap`) is not compiled and `BillingDependencies.Store`
  must name a store (Amazon, Steam, Stripe, Xsolla or your own).
- The core is not tied to any DI: the `IBillingService` interface and the `BillingService` class. There's a ready-made `BillingBootstrap` component and optional adapters for VContainer and Zenject.

## Installation

`Packages/manifest.json`:

```json
"com.yevhenii.purser": "https://github.com/yevhenii-sir/purser.git?path=/unity/com.yevhenii.purser"
```

To pin a specific version, append `#<tag or commit>` to the URL. For Google Play and the App Store add Unity IAP too
(Package Manager → Unity Registry → In-App Purchasing, 5.4 or later):

```json
"com.unity.purchasing": "5.4.4"
```

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
     optional `fallbackPrice` + `fallbackCurrency`, for subscriptions `fallbackPeriod` (`P1M`), and `access`: the access
     levels the product unlocks (`premium`, `no_ads`), as set for it on the server (the admin panel's config has them).
   - The fallback price is shown when the store gives none (Steam, Stripe, Xsolla, the Editor): `StoreProduct.Price`,
     `Currency` and `LocalizedPrice` (`0.99 USD`) are filled from it and `IsFallbackPrice` is set. It is never reported
     to the server as revenue. Copied into the admin panel, it becomes the default price of products that have none
     there; pasted back from the admin panel, the server's default prices fill it.
   - `StoreProduct.SubscriptionPeriod` (`P1M`; `SubscriptionPeriodDays` 30) shows "$4.99 / month" and compares plans.
     Google Play and the App Store (StoreKit 2 product details) report it; Steam, web stores and the Editor use
     `fallbackPeriod` and set `IsFallbackPeriod`. The admin panel's config fills it from the product's period.
   - The server address must be `https://` (plain `http://` only for `localhost`): outside the Editor the service
     refuses to initialize with any other URL. `requestTimeoutSeconds` (default 30) leaves room for a slow store API
     behind the server.
   - `entitlementsMaxAgeDays` (default 7): `IsActive`, `HasAccess` and `GetExpiration` count subscriptions and
     expiring access only from customer info received at most that many days ago by the device clock (and not with a
     clock set back before it was received), so an offline device cannot keep a subscription alive forever.
     Non-consumables and lifetime access are not affected; `0` turns the limit off.
   - `responseSigningPublicKey` (optional): see [Signed responses](#signed-responses).
3. The **Validate** and **Ping server** buttons in the config inspector check the settings and whether the server is
   reachable; **Open admin panel** opens the server's admin page (`{serverBaseUrl}/admin`).

### Profiles: dev, staging, prod

One config serves several environments, each a billing database of its own. A **profile** names an environment and
what differs there; everything else (the catalog, the behaviour) comes from the config and is written once:

- `serverBaseUrl`, `responseSigningPublicKey`: empty = the config's;
- `builds`: app ids per store for that environment (stores without a row use the config's builds);
- `productIds`: product ids that differ there, e.g. a test-mode Stripe Price id (`vip` / `stripe` / `price_test_…`);
- `isTest`: a test environment. **Validate** and release builds warn when the release profile is one.

Which profile runs: `editorProfile` in the Editor, `developmentProfile` in Development Builds, `releaseProfile` in
release builds (empty = the config's own fields). `config.UseProfile("staging")` picks one at run time, e.g. from a QA
menu, before the service and the store adapters are created (`""` = no profile, `null` = back to the rules).
`IBillingService.Profile` says which one runs; the log names it at startup.

The service, the API client and the store adapters apply the profile themselves (`config.Resolved()`), so pass them
the asset. The asset never changes at run time. A profile that does not exist stops the service with a
`BillingException`; the build checks it before building. The customer info and paywall caches belong to one server, so
switching profiles never shows another environment's purchases.

In the inspector, copy, paste, ping and **Open admin panel** use the Editor's profile: **Copy config for admin panel**
copies that environment's builds and ids, and **Paste config from admin panel** (from that environment's admin panel)
puts the server URL, builds and store ids that differ into the profile and the catalog into the shared config
(`PreviewJsonForProfile` / `MergeJsonIntoProfile` in code).

```csharp
var config = BillingConfig.Create("https://maincloud.spacetimedb.com/v1/database/mygame-billing/route", products);
config.profiles.Add(new BillingProfile("dev")
    .WithServer("https://maincloud.spacetimedb.com/v1/database/mygame-billing-dev/route")
    .WithBuild(BillingStores.Stripe, "com.example.mygame.web.dev")
    .WithProductId("vip", BillingStores.Stripe, "price_test_vip")
    .AsTest());
config.editorProfile = config.developmentProfile = "dev";
```

## Usage

### Without DI

**Tools → Purser → Add Bootstrap To Scene**, assign the config. Then in code:

```csharp
var billing = BillingBootstrap.Current.Service;
billing.SetUserId(playerId);              // after login: the player's identity hex / id in your backend

billing.PurchaseSucceeded += r => ShowReward(r.ProductId);    // also fires for purchases completed in the background
var result = await billing.PurchaseAsync("gems_100");
if (result.IsSuccess) { /* the server accepted it, the reward is in the game or on its way */ }

bool premium = billing.HasAccess("premium");                 // an access level: what the game checks
bool vip = billing.IsActive("vip");                          // a product: non-consumable or active subscription
DateTime? until = billing.GetExpiration("premium_monthly");
billing.CustomerInfoChanged += info => UpdateUi(info);       // only when what the player has changed
await billing.RestoreAsync();                                // "Restore purchases" button (required on iOS)
```

### Customer info and access levels

An **access level** is a name products unlock (`premium`, `no_ads`), set per product on the server: the monthly and
the yearly subscription and a lifetime purchase can all unlock `premium`, and the game checks `HasAccess("premium")`
instead of product ids. `billing.CustomerInfo` is what the server reported last (cached on the device for offline
starts, `IsFromCache`):

- `Access` — access level → `AccessInfo`; `Subscriptions` — catalog product id → `SubscriptionInfo`; both have
  `ProductId`, `Store`, `ExpiresAt` (UTC, grace period included; null = never), `PurchasedAt`, `WillRenew`,
  `IsInGracePeriod`, `HasBillingIssue` (ask the player to fix the payment method), `IsTrial`, `RenewsTo` (a pending
  downgrade), `IsFamilyShared` (Apple Family Sharing: a family member bought it and shares it with the player).
- `NonConsumables` — owned catalog product ids.
- `Paused` — catalog product id → `PausedSubscriptionInfo` (`ProductId`, `Store`, `ResumesAt`): a subscription the
  player paused on Google Play. It gives no access until it resumes, so it is not in `Subscriptions` and
  `IsActive` / `HasAccess` are false for it; show "paused until ..." instead of the offer to subscribe. (On the server
  the game events of a pause and of its end carry the reasons `paused` and `resumed`; the SDK reads the customer
  info, not those events.)

`RefreshCustomerInfoAsync()` asks the server again. The package refreshes by itself after initialization, after
purchases, when the app returns to the foreground (at most every 5 minutes), after the store reconnects, and a few
seconds after something in the customer info expired or a paused subscription was due to resume (while the game runs;
at most once a minute). One `/entitlements` request runs at a time: calls made meanwhile share one request that
follows it, and an answer older than the one already shown (by the server's clock) is dropped.
`CustomerInfoChanged` fires only when the items changed, not on every refresh. In the Editor without the server
(`editorMode = SkipServer`) purchases grant the config's `access` levels locally and keep them for the next session.

The user id is read again before server requests, when the app returns to the foreground and once a second, so a
`FuncUserIdProvider` whose id changes (login, logout, account switch) works like `SetUserId`: orders waiting for a
player are verified, and the new player's cached customer info is loaded.

If the store connection drops after initialization (`StoreDisconnected`), `IsInitialized` is false and the service
reconnects with backoff (5 s doubling to 5 minutes); `StoreReconnected` follows and the customer info is refreshed.

If the first `InitializeAsync()` cannot connect the store or load the products, its task fails and the service keeps
trying in the background with the same backoff, and right away when the app returns to the foreground; `Initialized`
fires when it succeeds. A configuration problem (a server URL without https, a platform without purchases) is not
retried.

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

bool noAds = billing.HasAccess("no_ads");                 // access levels — via the server's customer info
```

How this protects the reward:
- The server responds with `delivery: "client"` (`PurchaseResult.GrantOnClient`). For Google consumables it only acknowledges, so Google won't refund after 3 days, and leaves the consume to the client.
- The package fires `PurchaseSucceeded` first and finishes the purchase in the store only **after** the handlers: it consumes in Google or calls `finishTransaction` in Apple. If the game crashes before that, the store delivers the purchase again, the server answers `success` again, and the handler uses `TransactionId` to tell whether the reward was already granted.
- Grant the reward in `PurchaseSucceeded` and save it there too. The continuation of `await PurchaseAsync(...)` may run after the purchase has already been finished in the store.
- If a handler throws, the purchase isn't finished and the store delivers it again.
- If nobody listens yet (no `PurchaseSucceeded` handler and no `PurchaseAsync` waiting, e.g. a purchase the store
  delivers at launch before the shop is created), the purchase is kept and not finished: the first handler added to
  `PurchaseSucceeded` gets it right away, then it is finished.

After a reinstall the id will be new: non-consumables and subscriptions come back via `RestoreAsync()` (on Android the store returns them on startup anyway), and consumables don't need restoring.

### VContainer

The adapter is enabled automatically when `jp.hadashikick.vcontainer` is installed:

```csharp
builder.RegisterPurser(config);
```

If `IUserIdProvider` and `IAnalyticsIdProvider` are registered in the container, the adapter picks them up.

### Zenject / Extenject

Add the `PURSER_ZENJECT` scripting define and put `PurserInstaller` on a context (or call `PurserInstaller.Install(Container, config)`).

### Games that keep their own IAP code (observer mode)

A game with its own Unity IAP 4 or 5 code (or a shared purchase wrapper) keeps it and reports each purchase:

```csharp
var billing = Billing.Create(config, new BillingDependencies { Store = ExternalStoreAdapter.ForThisDevice(), ... });

// IAP 4 ProcessPurchase / IAP 5 OnPurchasePending, before the purchase is finished in the store:
_ = billing.ReportPurchaseAsync(ExternalPurchase.FromUnityReceipt(product.definition.storeSpecificId, receipt,
    product.metadata.localizedPrice, product.metadata.isoCurrencyCode));
```

The purchase is kept on the device until the server answered and verified with `client_finished` (the server does not
consume it). `PurchaseVerified` / `PurchaseRejected` report the outcome; `ReportMode.VerifyFirst` waits for it, with
`BillingConfig.failOpenAfterSeconds` as the limit. Builds of such games use delivery `none`. Sample **Observer mode**
and docs/observer-mode.md have the details.

### Google Play and Xsolla in one build

```csharp
var store = new CompositeStoreAdapter(Billing.CreateDefaultStore(config),
    new CurrencyStoreRouter(BillingStores.Xsolla, "RUB"),            // or a FuncStoreRouter (remote config)
    new XsollaStoreAdapter(config, new XsollaOptions { ReturnUrl = "mygame://purchase" }));
var billing = Billing.Create(config, new BillingDependencies { Store = store, ... });
```

Prices come from Google Play; purchases of players the router sends to Xsolla open Pay Station (Chrome Custom Tabs on
Android) and come back through the app link. The `xsolla` build in `BillingConfig.builds` is a build of the same app on
the server (platform Android, store Xsolla), so the customer info covers both. Items bought earlier through a
client-side Xsolla integration move once with `ClaimXsollaPurchasesAsync(xsollaLoginToken)`.

## How a purchase is processed

The store reports an order: a new purchase, a redelivery after restart, or a restore. The order is sent to `POST /verify/{google|apple}`.

| Server response | What the package does | `PurchaseStatus` |
|---|---|---|
| `success` | finish in the store, `PurchaseSucceeded` event; with `delivery: "client"` — the event first, then finish | `Success` |
| `pending` (202) | finish in the store, the server will deliver the reward | `PendingDelivery` |
| `pending_or_canceled` | doesn't confirm, the store will deliver the order again | `PendingPayment` |
| `invalid` / `forbidden` / `expired` / `revoked` | finish in the store as not granted (so the order doesn't keep coming back; Amazon: `UNAVAILABLE`), `PurchaseFailed` | `Rejected` |
| `error`, 5xx, `429`, no network | doesn't confirm, retries after `retryDelaysSeconds` (`429`: after its `Retry-After`), then when the app returns to the foreground, when the store reconnects and on the next launch | `PendingVerification` |

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

Cancelling the token passed to `PurchaseAsync` only stops waiting: the store purchase goes on and still counts for the
paywall when it completes.

## Subscription offers and changes

`StoreProduct.Offers` lists a subscription's offers with their pricing phases (`Price`, `LocalizedPrice`, `Period`,
`Cycles`, `IsFreeTrial`), and `IntroEligible` says whether the player can still get the introductory offer:

```csharp
var p = billing.GetProduct("premium_monthly");
if (p.IntroEligible == true && p.Offers.Any(o => o.HasFreeTrial))
    label.text = "Try free for a week, then " + p.LocalizedPrice;
```

What Unity IAP 5.4.4 exposes:
- **Google Play**: the one offer Unity IAP buys — the first offer Google Play lists for the product — with its free
  trial, introductory and base phases. Its offer id and offer token are not exposed (`Id` and `Token` are null) and
  another offer cannot be chosen. Google Play lists only offers the player is eligible for, so `IntroEligible` is true
  when the offer has a trial or intro phase and unknown otherwise.
- **App Store**: the subscription period, the introductory offer and StoreKit's `isEligibleForIntroOffer` (StoreKit 2).
  StoreKit applies the introductory offer by itself; promotional offers are bought with
  `PurchaseWithPromotionalOfferAsync`.

`PurchaseOptions` carries the paywall, the offer the player bought (`OfferId`, sent to the server as `offer_id` for
revenue by offer; it does not pick the offer) and a subscription the purchase replaces:

```csharp
await billing.PurchaseAsync("premium_yearly", new PurchaseOptions
{
    Paywall = paywall,
    OfferId = "yearly-intro",
    ReplacesProductId = "premium_monthly",                 // the player's current subscription
    ReplacementMode = ReplacementMode.ChargeProratedPrice, // Google Play; default WithTimeProration
});
```

On Google Play the change goes through `UpgradeDowngradeSubscription` with the owned order of the replaced
subscription; with `ReplacementMode.Deferred` nothing is charged now, the purchase reports `PendingPayment` and the
customer info shows `RenewsTo`. On the App Store a purchase in the same subscription group replaces the old one by
itself. Other stores fail the purchase.

### Prices per period, managing subscriptions, offer codes

`PricePer(unit)` converts a subscription's price to a day, week, month or year, and `LocalizedPricePer(unit)` formats
it like the store formatted `LocalizedPrice` (the same currency sign, separators and decimals; `FormatPrice(amount)`
does that for any amount). Pricing phases have the same two. `BillingPeriods.TryParse` splits a period into its count
and unit for your own localized label:

```csharp
var yearly = billing.GetProduct("premium_yearly");                     // $49.99, P1Y
label.text = $"{yearly.LocalizedPrice} / year, only {yearly.LocalizedPricePer(BillingPeriodUnit.Month)} a month";
if (BillingPeriods.TryParse(yearly.SubscriptionPeriod, out var count, out var unit))
    periodLabel.text = Localize(unit, count);                         // "1 year"
```

Stores require a way to manage a subscription from the app: `await OpenManageSubscriptionsAsync(productId)` opens
Google Play's page for that subscription, the App Store's subscriptions page or Stripe's customer portal, and returns
false when there is no such page (Amazon Appstore, Steam, Xsolla). On Stripe builds with player tokens the server
creates a portal session of the player's subscription; otherwise set `StripeCheckoutOptions.CustomerPortalLoginUrl`
to the portal's login link (docs/store-setup.md, Stripe → Customer portal). `PortalReturnUrl` (default `CancelUrl`)
is where the portal's Return link leads. On WebGL a page opened after an await may be blocked as a popup. `PresentOfferCodeRedemption()` shows the App Store's sheet for redeeming an
offer code (iOS 14 and later; false elsewhere); a redeemed code arrives as a purchase and is verified like any other.

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
  the order is remembered and checked again at the next launch, or now with `ResumePendingOrders()`. Such a
  resumed order is asked about three times (after `PollInterval`, then twice as long), not polled for the whole
  payment timeout; this goes for Stripe sessions too.

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
  (`/checkout/stripe`), opens its page (only an absolute `https://` page; anything else fails the purchase) (`StripeCheckoutOptions.OpenUrl`, default `Application.OpenURL`) and polls
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
  not answered yet are verified again at the next launch, and so is an order the player authorizes after the game
  was restarted. Prices are set on the server and shown by the overlay (`StoreProduct.LocalizedPrice` is empty).
  A bridge that also implements `ISteamWebApiTicketProvider` (the sample's does) sends a Steam Web API auth ticket
  for the identity `purser` (`ISteamUser::GetAuthTicketForWebApi`) with every checkout, so the server can check that
  the SteamID really belongs to the player. A server whose Steam build sets `require_ticket` (admin page → Delivery → Steam, or `admin_set_steam_build`) refuses checkouts
  without one: custom bridges must implement the interface before that setting is turned on.

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

## Player profile

Every request carries the device (`SystemInfo.deviceModel`, `SystemInfo.operatingSystem`), `Application.version` and
the package version (`PurserInfo.Version`); the admin page's player profile shows them with when the server first
and last heard from the player and the store country. Custom attributes go there too:

```csharp
await billing.SetCustomAttributesAsync(new Dictionary<string, object>
{
    ["email"] = playerEmail,   // only what the player agreed to share
    ["level"] = 12,
    ["old_flag"] = null,       // removes it
});
```

Keys are `a-z`, `0-9` and `_` (up to 40 characters, at most 30); values are strings, numbers or booleans.

## Binding a purchase to the player

With `bindPurchasesToUser = true` the package passes to the store:
- Google: `obfuscatedAccountId` = `user_id` (if longer than 64 characters — SHA-256 hex);
- Apple: `appAccountToken` = a UUID derived from SHA-256(`user_id`).

For consumables the server computes the same value and refuses (`403 forbidden`) if someone tries to credit the purchase to a different player. Non-consumables and subscriptions follow the store account: “Restore purchases” with a new `user_id` (reinstall, new device) moves them to the new player. The rule is shared with the server (`AccountIds` ↔ `purser_core::google_account_id` / `apple_account_token`) and checked with the same test vectors on both sides.

## App Store promotional offers

Discounts for current or lapsed subscribers, set up in App Store Connect (Subscription Prices → Promotional Offers):

```csharp
var result = await billing.PurchaseWithPromotionalOfferAsync("premium", "winback_50");
var fromPaywall = await billing.PurchaseWithPromotionalOfferAsync("premium", "winback_50", paywall); // counts for the A/B test
```

The server signs the offer for the current player (`POST /offers/apple/sign`, with the In-App Purchase key it already has), and the package buys through Unity IAP with the offer and the player's `appAccountToken`; the purchase is then verified like any other. App Store only: on Google Play it throws `BillingException` (Google subscription offers are picked on the client by their offer token). In the Editor the product is bought without the offer. `GetApplePromotionalOfferAsync` returns the signed offer (also as a V2 JWS) for your own StoreKit code. Unity IAP 5 has no public API for offers, so the package passes them to Unity IAP's StoreKit 2 bridge through its internals and fails rather than buying at the full price if they change. Details: [docs/apple-offers.md](../../docs/apple-offers.md).

## Signed client requests (player tokens)

A build can require that every client request is signed by the game's own backend: the backend makes an HS256 JWT for
the player with the build's client key, and the package sends it as `Authorization: Bearer <token>` with every request
(`/verify/*`, `/restore`, `/entitlements`, `/attributes`, `/paywall`, `/paywall/event`, `/offers/apple/sign`,
`/checkout/*`). Give the service an `IPlayerTokenProvider`:

```csharp
var tokens = new FuncPlayerTokenProvider((userId, ct) => myBackend.GetBillingTokenAsync(userId, ct));
var billing = Billing.Create(config, users, playerTokenProvider: tokens);
// or BillingDependencies.PlayerTokenProvider, the bootstrap's "Player Token Provider" field, or a DI registration
```

The token is kept per player until the server refuses it (`401 ERR_CLIENT_AUTH`: missing, expired or another
player's); then the provider is asked once more and the request is sent again once. A provider that throws is
treated like a network failure (purchases are verified again later). The service hands the provider to its API client
and to the Stripe, Xsolla and Steam adapters (`IPlayerTokenConsumer`).

## Rate limits

Any client route may answer `429` with `Retry-After`. Purchase verification then waits that long (at most 60 seconds)
instead of the next `retryDelaysSeconds`; a rate-limited answer never finishes or rejects a purchase. Paywalls fall
back to their cached answer; `ApiResult.RetryAfter` and `ApiResult.Code` (`ERR_RATE_LIMITED`) are there for custom
calls.

## Signed responses

With the build's response signing key on the admin page, the server signs `/entitlements` and `/restore` answers
(`X-Purser-Signature`: base64 Ed25519 over the exact body bytes). Put the public key (base64, 32 bytes) into
`BillingConfig.responseSigningPublicKey`: answers without a valid signature are then treated as errors and the cached
customer info is kept, so a proxy on the device's network cannot grant access. The check is built into
`BillingApiClient` (Ed25519 per RFC 8032 in C#, no native code; in a running game it is computed off the main thread,
except on WebGL); a custom `IBillingApi` has to do its own.

With a key, the customer info cache holds the signed answer itself (`ApiResult.RawBody` and `ApiResult.Signature`) and
its signature is checked again at every start: a cache that was edited on the device, saved without a signature or
signed for another player is ignored, and nothing is shown until the server answers. A custom `IBillingApi` that
does not fill `RawBody` / `Signature` therefore has no offline cache while a key is set.

## Storage and code stripping

`IBillingStorage` (default: `PlayerPrefsBillingStorage`) keeps the customer info and paywall caches, started web and
Steam orders and the refund data consent. Cache writes are batched: a storage that implements
`IFlushableBillingStorage` (PlayerPrefs does) is flushed to disk when the app is paused or quits and when the service is
disposed, and at once after a write that must not be lost (a started order, the consent). An answer equal to the
cached one is written again at most once an hour, and the device keeps the cache of the last player only.

`Runtime/link.xml` keeps the types Newtonsoft.Json creates by reflection and the Unity IAP internals of the App Store
promotional offer bridge from managed code stripping (IL2CPP, a stripping level above Minimal). Unity reads `link.xml`
files from `Assets` only, so the package's Editor assembly hands this one to the linker during a build; nothing to set
up.

## GA4, AppsFlyer, Adjust, Tenjin, AppMetrica, Amplitude, Mixpanel, Meta, Apple Ads

The server sends purchase events to the connected integrations itself. The client only has to pass device identifiers:

- **GA4:** implement `IAnalyticsIdProvider` (for example via `FirebaseAnalytics.GetAnalyticsInstanceIdAsync()`) so events are tied to the right `app_instance_id`.
- **AppsFlyer:** pass `appsflyer_id` via `IAttributionProvider`. AppsFlyer won't accept the event without it.
- **Tenjin:** pass `tenjin_id` (the Tenjin SDK's analytics installation id) and `os_version`, plus `advertising_id` / `idfa` and `idfv` when the player consented.
- **Adjust:** pass `adjust_id` (`Adjust.GetAdid()`), or at least `advertising_id` / `idfa` / `idfv`.
- **AppMetrica:** pass `appmetrica_device_id` (from `AppMetrica.RequestAppMetricaDeviceID`), or set the AppMetrica profile id to the billing user id and turn on "profile id" in the integration.
- **Amplitude:** nothing required: events go to the billing user id (use it as the Amplitude user id in the game). Optionally pass `amplitude_device_id` (`AttributionKeys.AmplitudeDeviceId`) to tie them to the device.
- **Mixpanel:** nothing: events go to the billing user id as `distinct_id` (identify players by it in the game).
- **Meta Conversions API:** pass `meta_anon_id` (`AttributionKeys.MetaAnonId`, the Facebook SDK's anonymous id) or an advertising id (`advertising_id`; on iOS `idfa` with `att` = "true"), plus `os_version` and `app_version`.
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
