# Changelog

## [Unreleased]

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
