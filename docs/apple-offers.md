# App Store subscription offers

The App Store has four kinds of subscription offers:

| Offer | Who gets it | Server signature | Supported here |
|---|---|---|---|
| Introductory offer | New subscribers of the group | no | yes, it's just a purchase |
| **Promotional offer** | Current and lapsed subscribers your app chooses | **yes** | `POST /offers/apple/sign` + Unity `PurchaseWithPromotionalOfferAsync` |
| Offer code | Whoever redeems the code | no | yes, the redeemed transaction is verified like any purchase |
| Win-back offer | Lapsed subscribers the App Store picks | no | yes when the App Store shows it; buying it from the app needs your own StoreKit code |

Promotional offers are the only kind that needs your server: StoreKit accepts the discount only with a
signature made with your In-App Purchase key, so the key never ships inside the app. The billing server
already has that key for verifying purchases.

## Set up in App Store Connect

1. **In-App Purchase key.** The same key as for verification (see [store-setup.md](store-setup.md#in-app-purchase-key)):
   `apple_issuer_id`, `apple_key_id`, `apple_private_key_p8`. Secrets resolve build → developer account →
   `default`, so a build published from another account signs with that account's key. `apple_issuer_id` is
   only needed for the V2 signature.
2. **The offer.** Your app → Monetization → Subscriptions → the subscription → Subscription Prices →
   **Promotional Offers** → Create. The **Promotional Offer Identifier** (e.g. `winback_50`) is the `offer_id`
   you pass to the server. Pick the type (pay as you go, pay up front, free) and the duration.
3. Add the subscription to the server catalog (Products) as kind `subscription`, with the same product id.

The App Store only lets customers who have, or had, a subscription in the same subscription group redeem a
promotional offer. Who sees the offer in your app is up to you (a churn screen, a support gift, a lapsed
player coming back).

## Server: `POST /offers/apple/sign`

```json
{
  "bundle_id": "com.example.mygame",
  "product_id": "premium_monthly",
  "offer_id": "winback_50",
  "user_id": "<player id>",
  "transaction_id": "<optional: any App Store transaction of the player, for the V2 JWS>"
}
```

The server checks that the build exists, is enabled and is an iOS build, and that the product belongs to
it and is a subscription. Answer:

```json
{
  "status": "success",
  "bundle_id": "com.example.mygame",
  "product_id": "premium_monthly",
  "offer_id": "winback_50",
  "key_id": "<In-App Purchase key id>",
  "nonce": "8f4c0a62-1b9e-8d43-a2c1-5e7f9b0d3a14",
  "timestamp": 1790000000123,
  "signature": "MEUCIQ...",
  "app_account_token": "c3214538-d133-89d9-b4db-63a435cddcae",
  "signed_offer_jws": "eyJhbGciOiJFUzI1NiIs...",
  "expires_at_ms": 1790086400123
}
```

| HTTP | `status` | Meaning |
|---|---|---|
| 200 | `success` | Signed. Use it for one purchase attempt within 24 hours. |
| 400 | `error` | Unknown or disabled build, Android build, unknown product, not a subscription, invalid ids. |
| 500 | `error` | `[ERR_OFFER_KEY]`: the In-App Purchase key is missing or invalid for this build (the log names the missing secret). |

- **`signature`** is the classic promotional offer signature
  ([Generating a signature for promotional offers](https://developer.apple.com/documentation/storekit/generating-a-signature-for-promotional-offers)):
  ECDSA P-256 / SHA-256 over `bundle_id ⁣ key_id ⁣ product_id ⁣ offer_id ⁣ app_account_token ⁣ nonce ⁣ timestamp`
  joined with U+2063, DER, base64. It works with StoreKit 1 (`SKPaymentDiscount`) and StoreKit 2
  (`Product.PurchaseOption.promotionalOffer(offerID:keyID:nonce:signature:timestamp:)`).
- **`app_account_token`** is the player's `appAccountToken` (`purser_core::apple_account_token(user_id)`,
  the same binding as purchases). The signature covers it, so **the purchase must be made with this token**,
  otherwise StoreKit rejects the offer.
- **`signed_offer_jws`** is the promotional offer V2 signature
  ([Generating JWS to sign App Store requests](https://developer.apple.com/documentation/storekit/generating-jws-to-sign-app-store-requests)):
  an ES256 JWS with `aud: "promotional-offer"`, `iss`, `iat`, `bid`, `nonce`, `productId`, `offerIdentifier`
  and the optional `transactionId`, for StoreKit 2 `Product.PurchaseOption.promotionalOffer(_:compactJWS:)`
  on newer iOS versions. `null` when `apple_issuer_id` is not set.
- **`nonce`**: Apple accepts every nonce once. The server derives it with HMAC-SHA256 keyed by the `.p8`
  key over a counter that grows with every signature (`server_state.promotional_offer_nonce_counter`) and the
  request, so it is unique and cannot be guessed. The module's random generator is seeded by the timestamp
  and is not used.
- No secret is returned or logged. The key id is not a secret: StoreKit needs it.
- The route is public like `/verify`: anyone who knows a player id can get a signature for that player.
  That only lets them buy the discounted subscription for that player's account token, and the App Store
  still limits promotional offers to current and lapsed subscribers.

## Unity

```csharp
// App Store only. The server signs the offer for the current player, then the purchase goes through
// Unity IAP and /verify/apple like any other purchase.
var result = await billing.PurchaseWithPromotionalOfferAsync("premium", "winback_50");
if (result.IsSuccess) { /* subscribed with the discount */ }
```

- `productId` is the id from `BillingConfig` (or the App Store id); `offerId` is the Promotional Offer Identifier.
- Results are the usual `PurchaseResult`s: `Cancelled`, `Failed` (with the reason, e.g. `[ERR_OFFER_KEY]`),
  `Success`... A purchase of the product that is already running returns `AlreadyInProgress`.
- On Google Play it throws `BillingException` (see below). In the Editor it buys the product without the
  offer.
- `GetApplePromotionalOfferAsync(productId, offerId)` returns the signed offer (with `SignedJws`) for games
  that buy through their own StoreKit code. Each offer is good for one purchase attempt.

**How it reaches StoreKit.** Unity IAP 5 has no public API for promotional offers, but its StoreKit 2 bridge
(`PurchaseProductOptionsConverter.swift`) builds `Product.PurchaseOption.promotionalOffer` from the purchase
options JSON. The package calls the internal `AppleStoreImpl.Purchase(ProductDefinition, optionsJson)` by
reflection with the offer and the server's `appAccountToken` (checked against the sources of Unity IAP 5.0.1,
5.3.1 and 5.4.4). If the internals are missing (another Unity IAP version, or StoreKit 1 on iOS older than 15),
the purchase fails with a clear reason. It never buys at the full price instead: an offer StoreKit cannot
parse would be dropped silently, so the package validates the offer before starting the purchase.

A custom `IStoreAdapter` supports offers by implementing `IApplePromotionalOfferStore`; a custom `IBillingApi`
by implementing `IApplePromotionalOfferApi`.

## Google Play

Google Play has no server-signed offers. Subscription offers (developer-determined offers on a base plan)
are chosen on the client: the app picks an offer of the product and passes its **offer token** to the billing
flow (`ProductDetails.SubscriptionOfferDetails.offerToken`). Eligibility rules and tags are set in Play
Console. The purchase is verified by `/verify/google` like any other.

## Offer codes and win-back offers

- **Offer codes**: Unity IAP shows Apple's redemption sheet with
  `StoreController.AppleStoreExtendedPurchaseService.PresentCodeRedemptionSheet()`. Codes redeemed there or in
  the App Store arrive as ordinary transactions and are verified by the package and `/verify/apple`;
  App Store Server Notifications cover renewals.
- **Win-back offers** need no signature. The App Store shows them to eligible lapsed subscribers itself (in the
  App Store and through StoreKit messages); the purchase reaches the app as a normal transaction. Offering one
  from inside the app needs StoreKit 2 `Product.PurchaseOption.winBackOffer(_:)` (iOS 18+), which Unity IAP 5
  does not expose: use your own StoreKit code for that.
