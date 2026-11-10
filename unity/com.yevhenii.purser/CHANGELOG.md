# Changelog

## [Unreleased]

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
