# Changelog

## [Unreleased]

## [0.9.0] - 2026-11-24

### Added

- Paused subscriptions and Family Sharing (needs a server that reports them): `CustomerInfo.Paused` (catalog product
  id → `PausedSubscriptionInfo`: `ProductId`, `Store`, `ResumesAt`) lists subscriptions the player paused on Google
  Play; they give no access until they resume, so `IsActive` / `HasAccess` are false for them. `IsFamilyShared` on
  `SubscriptionInfo` and `AccessInfo`: Apple Family Sharing, a family member shares the purchase. DTOs:
  `CustomerInfoDto.Paused` (`paused`, `PausedStateDto`), `EntitlementStateDto.FamilyShared` (`family_shared`). Both
  count for `CustomerInfoChanged` and are kept in the cache.
- `IFlushableBillingStorage` (`Flush()`): a storage that batches its writes. `PlayerPrefsBillingStorage` implements it.
- `ApiResult.RawBody` / `ApiResult.Signature`: the signed bytes of a `/entitlements` or `/restore` answer, for the
  customer info cache.
- `BillingLog.IsEnabled(level)`.
- `ServerCheckoutStoreAdapter` (for adapters derived from it): `RaiseResumed`, `CheckAsync`, `ResumeAttempts`,
  `MaxFailedRequests`.
- `Runtime/link.xml` (and `PurserLinkerProcessor` in the Editor assembly, which gives it to the Unity linker): the
  types Newtonsoft.Json creates by reflection and the Unity IAP types of the promotional offer bridge survive managed
  code stripping.
- `AttributionKeys.AmplitudeDeviceId`, `AttributionKeys.MetaAnonId` for the server's Amplitude and Meta Conversions API integrations.

### Fixed

- A purchase the game grants itself (`delivery: "client"`, `PurchaseResult.GrantOnClient`) was finished in the store
  even when nobody received it (no `PurchaseSucceeded` handler and no `PurchaseAsync` waiting, e.g. a purchase the
  store delivers at launch): the reward was lost. It is now kept, not finished, and given to the first handler added
  to `PurchaseSucceeded`, then finished; a handler that throws leaves it for the next one.
- An `Initialized` handler that throws no longer fails the initialization (the waiting orders and the first customer
  info refresh were skipped, and a later `InitializeAsync` connected the store again).
- A provider of the game that never answers (`IAnalyticsIdProvider`, `IAttributionProvider`, `IPlayerTokenProvider`,
  Apple Ads attribution) no longer blocks purchase verification: after a few seconds the request goes without its
  value (a warning is logged once).
- An order whose verification was given up (the server could not be reached) is verified again when the app returns
  to the foreground and when the store reconnects, not only on the next launch. Its paywall and offer stay with it and
  are no longer sent with later orders of the same product.
- `RestoreAsync` waits (at most one request timeout) for the purchases being verified, so the customer info it returns
  includes them.
- A customer info refresh that failed no longer counts for the five-minute foreground throttle: the next return to
  the foreground tries again.
- An older order of a product (one being verified, waiting for a player or given up when `PurchaseAsync` was called)
  no longer completes the `PurchaseAsync` of a new purchase of that product, and does not take its paywall. Its result
  still arrives through the events.
- After `Dispose`, `IsInitialized` is false, `RestoreAsync` and `RefreshCustomerInfoAsync` fail with
  `ObjectDisposedException` and `PurchaseAsync` answers `NotInitialized` instead of calling the store or the server.
- A store disconnect raised while a reconnection was finishing (inside `StoreReconnected` handlers) was ignored and
  the service stayed "initialized" on a dead connection; it reconnects again now.
- `UnityWebRequestTransport`: a request cancelled from another thread is aborted on the main thread.
- An order the store delivers while the products are still loading (unfinished purchases right after the store
  connects) was verified with price 0 and no currency; it waits for the products now (at most 10 s).

### Changed

- `InitializeAsync`: when the store cannot be connected or the products cannot be loaded, the returned task still
  fails, but the service keeps trying in the background (5 s doubling to 5 minutes, and right away when the app returns
  to the foreground) and raises `Initialized` when it succeeds. Configuration problems (server URL without https,
  unsupported platform) are not retried. The hidden lifecycle object is created at the first `InitializeAsync`.
- Checking a signed response takes about a third less time (one interleaved double-scalar multiplication, a decoded
  public key that is kept, a cheaper modular reduction) and, in a running game, is done off the main thread (not on
  WebGL).
- With `responseSigningPublicKey`, the customer info cache holds the signed answer and its signature, and the
  signature is checked again when the cache is read: a cache edited on the device, saved without a signature or
  signed for another player is ignored. Caches written by earlier versions with a key set are not read (the first
  answer of the server replaces them). The cache is read once at startup, not twice.
- `RefreshCustomerInfoAsync`: one `/entitlements` request runs at a time; calls made while it runs share one request
  that follows it. An answer older than the customer info already shown (by server time), from `/entitlements` or
  `/restore`, is dropped. Cancelling the token stops waiting; the request goes on.
- The customer info is refreshed a few seconds after something in it expired or a paused subscription was due to
  resume (checked once a second in play mode, at most one such refresh a minute).
- `PlayerPrefsBillingStorage` no longer calls `PlayerPrefs.Save()` on every write: the service flushes it when the app
  is paused or quits and when it is disposed, and right after a started web / Steam order, a fulfilled Amazon receipt
  and the refund data consent. A customer info or paywall answer equal to the cached one is written again at most once
  an hour. When another player's data is cached, the previous player's cache entries are emptied.
- Web checkout (Stripe, Xsolla): an order resumed from an earlier launch, by `ResumePendingOrders()` or by
  `StripeStoreAdapter.ResumeSession` is asked about three times with backoff instead of being polled for the whole
  payment timeout. Three failed requests in a row (no network, 5xx) end a poll with that error, which the service
  retries on its `retryDelaysSeconds`.
- `GetProduct`, `IsActive` and `GetExpiration` use lookups built at initialization instead of searching the product
  lists on every call; log messages below the configured level are no longer built.

## [0.8.1] - 2026-11-20

No package changes: the version follows the server (SpacetimeDB 2.16.1, table cache policies), which works with this package as before.

## [0.8.0] - 2026-11-16

### Breaking

This version needs the Purser server of the same version (new `/entitlements` answer, `offer_id`, player tokens,
signed responses).

- **Customer info replaces entitlements.** `Entitlements` is removed; `IBillingService.CustomerInfo` (`CustomerInfo`:
  `Access` level name → `AccessInfo`, `Subscriptions` catalog id → `SubscriptionInfo`, `NonConsumables`, server time,
  fetch time, `IsFromCache`, `IsFresh`) takes its place. `AccessInfo` / `SubscriptionInfo` have `ProductId`, `Store`,
  `ExpiresAt` (UTC, grace period included, null = never), `PurchasedAt`, `WillRenew`, `IsInGracePeriod`,
  `HasBillingIssue`, `IsTrial`, `RenewsTo`. `EntitlementsChanged` → `CustomerInfoChanged`, raised only when the items
  changed; `RefreshEntitlementsAsync` → `RefreshCustomerInfoAsync`; `RestoreAsync` returns `CustomerInfo`. DTOs:
  `EntitlementsDto` → `CustomerInfoDto` + `EntitlementStateDto`, `RestoreResponse.Entitlements` → `CustomerInfo`
  (JSON `entitlements`), `IBillingApi.EntitlementsAsync` returns `ApiResult<CustomerInfoDto>`.
- The customer info cache is stored under a new key (`purser.customer_info.<app>.<user>`); the old entitlements cache
  is not read.
- `IHttpTransport.SendAsync` takes request headers; `HttpResponse` carries the response headers and raw body bytes.
- `IStoreAdapter` implementations: subscription offers are new optional `StoreProduct` constructor arguments.

### Added

- Access levels: `IBillingService.HasAccess(level)`. `BillingProductConfig.access` (JSON `access`, merged from and
  written to the admin panel's config) lets the Editor without the server (`SkipServer`) grant them; local grants are
  cached for the next Editor session.
- Subscription offers: `StoreProduct.Offers` (`StoreOffer`: `Id`, `Token`, `PricingPhases` with `Price`,
  `LocalizedPrice`, `Currency`, `Period`, `Cycles`, `IsFreeTrial`) and `StoreProduct.IntroEligible`. Google Play: the
  offer Unity IAP 5 buys (its id and token are not exposed by Unity IAP). App Store: the introductory offer,
  `isEligibleForIntroOffer`, and the subscription period (no longer only the config's fallback on iOS).
- `PurchaseAsync(productId, PurchaseOptions)`: `Paywall`, `OfferId` (sent to `/verify` as `offer_id`),
  `ReplacesProductId` + `ReplacementMode` (Google Play subscription upgrade / downgrade through
  `UpgradeDowngradeSubscription`; `Deferred` reports `PendingPayment` until the renewal). New optional adapter
  interface `ISubscriptionChangeStore`.
- `PurchaseWithPromotionalOfferAsync(productId, offerId, paywall)`: promotional offer purchases count in A/B test
  results; the offer id is sent as `offer_id`.
- Player tokens for builds that require signed client requests: `IPlayerTokenProvider` (`FuncPlayerTokenProvider`)
  in `BillingDependencies.PlayerTokenProvider`, `Billing.Create(..., playerTokenProvider)`, the bootstrap's
  "Player Token Provider" field and the VContainer / Zenject registrations. Every client request carries
  `Authorization: Bearer <token>`; on `401 ERR_CLIENT_AUTH` the provider is asked once more and the request is retried
  once. `IPlayerTokenConsumer` is implemented by `BillingApiClient` and the Stripe, Xsolla and Steam adapters.
- Rate limits: a `429` is retried after its `Retry-After` (at most 60 s) and never finishes a purchase;
  `ApiResult.Code`, `ApiResult.RetryAfter`, `ApiResult.IsRateLimited`, `ErrorCodes`. Paywalls use their cache on `429`.
- Signed responses: `BillingConfig.responseSigningPublicKey` (base64 Ed25519). When set, `/entitlements` and
  `/restore` answers without a valid `X-Purser-Signature` are errors and the cached customer info is kept. Ed25519
  verification is implemented in C# (RFC 8032, tested with its vectors).
- Store connection: `IBillingService.StoreDisconnected` / `StoreReconnected`. When Unity IAP reports a disconnect after
  initialization, `IsInitialized` turns false and the service reconnects with backoff (5 s doubling to 5 minutes),
  then refreshes the customer info. New optional adapter interface `IStoreConnectionEvents`.
- The customer info is refreshed when the app returns to the foreground (at most every 5 minutes).

### Fixed

- `GetExpiration` applies the same `entitlementsMaxAgeDays` limit as `IsActive`.
- `IsActive` / `GetExpiration` by a store product id (`premium_android`) find a subscription keyed by the catalog id.
- A changed id of the user id provider (e.g. `FuncUserIdProvider`) is noticed: it is read again before server
  requests, on resume and once a second, and a change works like `SetUserId` (waiting orders are verified, the new
  player's cache is loaded).
- Cancelling the token of `PurchaseAsync` no longer drops the paywall of the store purchase, which still counts for
  the A/B test when it completes.
- Apple restore: an order re-delivered by `RestoreTransactions` while it is verified is not verified again (in-flight
  orders are tracked by key and transaction id) and is not posted to `/restore` too.

## [0.7.0] - 2026-11-15

### Breaking

- `/verify/<store>` and `/offers/apple/sign` send the build's id as `app_id` instead of `bundle_id`, like every other
  request; this needs a server of the same version. The C# fields `VerifyRequest.BundleId` and
  `ApplePromotionalOfferRequest.BundleId` are renamed to `AppId`.
- `BillingConfig.MergeJson` no longer detects the pre-0.2.0 config format (`androidAppId`, `iosAppId`, ...): such
  keys are ignored. Copy the config from the admin panel.

## [0.6.0] - 2026-11-12

### Security

- Outside the Editor the service refuses to initialize when `serverBaseUrl` is not `https://` (plain `http://` only
  for `localhost` / loopback addresses), and `BillingApiClient` and the Steam / Xsolla adapters refuse to send to such
  a URL. `BillingConfig.Validate` accepts `http://localhost`.
- Stripe and Xsolla open only an absolute `https://` payment page returned by the server; anything else fails the
  purchase.
- Entitlements: a device clock set back no longer makes time run backwards (`Entitlements.EstimatedServerNowMs`
  never goes below the server's time). New `BillingConfig.entitlementsMaxAgeDays` (default 7, `0` = no limit):
  `IsActive` ignores subscriptions from entitlements older than that by the device clock, or received "in the future"
  (clock set back), until the next refresh. Non-consumables are not affected. `Entitlements.IsFresh`.
- Steam: new optional `ISteamWebApiTicketProvider` on the bridge. When the bridge implements it, every
  `/checkout/steam` carries `ticket`, the hex-encoded Steam Web API auth ticket for the identity `purser`
  (`SteamStoreAdapter.WebApiTicketIdentity`); `SteamOptions.TicketTimeout` (10 s) bounds the wait. The Steam sample's
  bridge implements it with Steamworks.NET (20.2+). Servers can require the ticket with the Steam build setting
  `steam_require_ticket`: update custom bridges before turning it on.

### Changed

- Default request timeout is 30 s (`BillingConfig.requestTimeoutSeconds`, `BillingConfig.DefaultRequestTimeoutSeconds`,
  and the defaults of `BillingApiClient`, `SteamStoreAdapter` and `XsollaStoreAdapter`): the server may wait for a
  slow store API. Existing config assets keep their saved value; raise it from 10.
- `ServerCheckoutStoreAdapter` cancels running checkouts and polls on `Dispose` (`Lifetime`, `IsDisposed` for derived
  adapters; overrides of `Dispose` call the base).
- `Billing.InitializeInBackground(service)` starts initialization and logs a failure; `BillingBootstrap` and the
  VContainer / Zenject entry points use it.
- Failures to read or write the local caches (entitlements, paywalls, Amazon receipts, Apple Ads state) are logged as
  warnings instead of being ignored; `AppleAdsAttributionProvider` takes an optional `BillingLog`.
- XML docs for `BillingLog`, `IBillingStorage`, `PurchaseStatus`, `PurchaseResult`, `HttpResponse`, `ApiResult` and
  the Amazon types.

### Fixed

- Steam: an order checked out before the game restarted and authorized after it is verified (it was dropped unless
  another purchase started first). Checked-out orders are remembered under `purser.steam.started`.
- `BillingService.Dispose` disposes its cancellation source; an Apple Ads attribution failure at launch is logged
  instead of being left unobserved.
- Basic Shop sample: unsubscribes from the service in `OnDestroy` and reports purchase / restore errors.

## [0.5.0] - 2026-11-11

- Subscription periods: `StoreProduct.SubscriptionPeriod` (ISO 8601, `P1M`), `SubscriptionPeriodDays` and
  `IsFallbackPeriod`. Google Play reports the period through Unity IAP; elsewhere (the App Store, whose period Unity IAP 5
  keeps internal, Steam, web stores, the Editor) it comes from the new `BillingProductConfig.fallbackPeriod`
  (`WithFallbackPeriod`). The config JSON carries it as `period`, so pasting the admin panel's config fills it.
  `BillingPeriods` parses ISO periods. No server change is needed: nothing new is sent.
- Works with servers that key paywalls and entitlements by the app's catalog id (the config's product `id`): no code
  change, covered by tests.

## [0.4.0] - 2026-11-11

- No SDK changes: released together with the server 0.4.0 (products created in the stores from the admin page, admin
  roles, audit log). Games on 0.3.0 keep working unchanged.

## [0.3.0] - 2026-11-10

- Player profile: every request carries the device model, OS, app version and SDK version (`client`; the admin page
  shows them with first / last seen and the store country). `IBillingService.SetCustomAttributesAsync` sets custom
  attributes of the player (`POST /attributes`). `IBillingApi` gains `AttributesAsync` (custom implementations must
  add it); `PurserInfo.Version`.

- **Paste config from admin panel** shows what changes before applying, and merges instead of replacing the lists:
  **Add and update** (default: adds and updates, keeps what only Unity has) or **Match the admin panel** (also removes
  builds, products and store ids the admin panel does not have). Products are matched by id or by any store id.
  `BillingConfig.PreviewJson` / `MergeJson(json, ConfigMergeMode)` return the `ConfigChange` list; `ApplyJson` is
  removed.

- Fallback prices: `BillingProductConfig.fallbackPrice` / `fallbackCurrency` (`WithFallbackPrice`) fill
  `StoreProduct.Price`, `Currency` and `LocalizedPrice` when the store gives no price (Steam, Stripe, Xsolla, the
  Editor); `StoreProduct.IsFallbackPrice` marks them, and they are not sent to the server as the purchase price. The
  config JSON carries them as `price` / `currency` in both directions.
- **Open admin panel** in the config inspector.

## [0.2.0] - 2026-11-10

### Breaking

- `BillingConfig` is keyed by the server's store id instead of fixed per-platform fields: `builds` (`{store, appId}`,
  `google` / `apple` / `amazon` / `steam` / `stripe` / `xsolla`) replaces `androidAppId`, `iosAppId`, `amazonAppId`,
  `steamAppId` and `webAppId`, and a product's `storeIds` (`{store, id}`) replaces `googleId`, `appleId` and
  `amazonId`. A web game can now sell through both Stripe and Xsolla (each adapter sends its own build's app id), and
  Steam and web products can have their own store ids. Existing assets lose the old values: enter the builds again or
  paste the config from the admin panel. `AppIdFor(string store)`, `WithBuild`, `WithStoreId`, `AllIds()`.
- `IStoreAdapter` names its store (`Store`) and replaces `IStoreNameProvider`; `Confirm(order)` is
  `Finish(order, granted)` and `OrderConfirmed` is `OrderFinished`; `SetAccountBinding(userId)` (the Unity IAP adapter
  derives the Google and Apple ids itself). `IBillingApi.VerifyAsync(string store, ...)`; `VerifyRequest.Store`,
  `BillingPlatform.StoreName()` / `RouteName()`, `StoreOrder.AccountId` / `OriginalTransactionId` are removed
  (`BillingStores.Of(platform)`). `IBillingService.Store`.
- Server checkouts: `ServerCheckoutStoreAdapter` is the base of the Steam, Stripe and Xsolla adapters (remembered
  orders, config products, polling) and of other stores whose purchase starts on the server. `ICheckoutStoreAdapter`
  (`PrepareCheckout(CheckoutContext)`) replaces `IBillingUserStoreAdapter`; `IWebhookStoreAdapter.CheckOrderAsync`
  receives the verify request. Their constructors lost the `delay` / `utcNow` test parameters.
- `LocalUserIdProvider.DefaultKey` is `purser.user_id`. `ReceiptParser` is internal.

### Added

- **Copy config for admin panel** in the config inspector (`BillingConfig.ToJson()`): paste it in the admin panel to
  create the builds and products of a game that was built first. A store popup for builds and product store ids.
- `BillingBootstrap` takes an `IStoreAdapterFactory` component, and the VContainer / Zenject registrations a bound
  `IStoreAdapter`, so Amazon, Steam, Stripe and Xsolla work through them.

### Fixed

- A purchase whose store does not name the product (a Steam order authorized before a restart, a resumed Stripe
  session) was reported as the first configured product when the config came from the inspector (empty per-store
  ids matched an empty id); it is now the product named by the server's answer.
- Amazon: a receipt the server rejected was reported to Amazon as fulfilled; it is now `UNAVAILABLE`.
- Stripe and Xsolla purchases did not carry the attribution ids, the Firebase app instance id and the paywall (A/B
  test results); the Stripe session check and the Xsolla checkout now send them.
- A store failure that named no product left `PurchaseAsync` waiting forever when several purchases were open.
- Exceptions thrown by the game's `PurchaseSucceeded` / `PurchasePending` / `PurchaseFailed` handlers no longer
  reach the store callbacks, and a throwing `PurchaseSucceeded` handler is no longer re-reported as pending.
- Unreadable remembered Steam / Stripe / Xsolla orders are kept under a `.corrupt` key instead of being overwritten.
- Amazon: "Restore purchases" right after start could lose unfinished purchases of earlier sessions (both requests
  shared Amazon's one updates stream); it now waits for the startup request.
- No store calls or `Initialized` after `Dispose` while a request was running.

## [0.1.0] - 2026-11-10

First public release.

- `SetRefundDataConsent(bool)` and `AttributionKeys.RefundDataConsent`: the player's consent to share consumption data with Apple when they ask for a refund. Stored on the device and sent with every verification and restore; the server answers Apple's refund requests only for players who consented. `IBillingService` gains the method (custom implementations must add it).
- Web and PC stores: `StripeStoreAdapter` and `XsollaStoreAdapter` (open the checkout page, wait for the server's grant, resume unpaid orders at the next launch) through `IWebhookStoreAdapter`; `SteamStoreAdapter` with `ISteamMicroTxnBridge` (sample "Steam": Steamworks.NET bridge behind `PURSER_STEAMWORKS_NET`). `IBillingUserStoreAdapter` (SetBillingUser).
- Paywalls: `GetPaywallAsync(placementId)` (products with localized prices, remote config, variant and test ids; cached for offline starts), `PurchaseAsync(productId, paywall)`, `LogPaywallShownAsync(paywall)`, `PaywallNotFoundException`. `IBillingApi` gains `PaywallAsync` / `PaywallEventAsync` (custom implementations must add them); `BillingException` is no longer sealed.
- App Store promotional offers: `PurchaseWithPromotionalOfferAsync(productId, offerId)` and `GetApplePromotionalOfferAsync`; optional interfaces `IApplePromotionalOfferStore`, `IApplePromotionalOfferApi`.
- Amazon Appstore: `AmazonStoreAdapter` with a sample bridge to Amazon's Unity plugin; `BillingPlatform.Amazon` / `Steam` / `Web`, `IStoreNameProvider`, config `amazonAppId` / `steamAppId` / `webAppId`, `androidStore`, per-product `amazonId`; requests carry `store_user_id`.
- `BillingConfig.collectAppleAdsAttribution` / `AppleAdsAttributionProvider`: on iOS 14.3+ the Apple Ads campaign of
  the install is resolved once on the device (AdServices) and sent with purchases (`asa_*` attribution keys).
- `AttributionKeys.AdjustId`, `AttributionKeys.AppMetricaDeviceId` for the server's Adjust and AppMetrica integrations.
- Local delivery (server `delivery = none`, `PurchaseResult.GrantOnClient`): `PurchaseSucceeded` is raised before the
  store purchase is finished (Google consume / Apple finishTransaction), so a crash before the game granted the
  reward makes the store deliver the purchase again. A throwing handler leaves the purchase unfinished.
- `LocalUserIdProvider`: anonymous persistent user id for games without accounts.
- Server: account binding is enforced only for consumables; non-consumables and subscriptions can be restored
  on a new user id (reinstall, new device).
- First version: Unity IAP 5 store adapter, server verification (`/verify`, `/restore`, `/entitlements`),
  retries, entitlements cache, account binding (Google obfuscatedAccountId / Apple appAccountToken),
  `BillingConfig` asset, `BillingBootstrap`, VContainer and Zenject adapters, EditMode tests, Basic Shop sample.
