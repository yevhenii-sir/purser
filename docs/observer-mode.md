# Observer mode: games with their own IAP code

Most games already have purchase code: Unity IAP 4 or 5, a shared wrapper, analytics SDKs that report first purchases.
Observer mode adds Purser next to that code without replacing it. The game keeps buying, granting and finishing
purchases as before; Purser records each purchase, verifies it with the store and follows it afterwards.

What the game gets that it cannot see from the client:

- renewals, cancellations, billing problems, pauses and refunds of subscriptions (the stores tell the server);
- refunds of one-time purchases, so the game can take back what a refunded purchase gave;
- forged receipts refused by the server;
- revenue of all games in one place, and those events sent to Tenjin, GA4, AppMetrica, Adjust or AppsFlyer
  from the server.

## 1. The server

For each store build of the game, add a build with **delivery `none`**: the game grants, the server records. Keys,
products and store notifications as in [Store setup](store-setup.md); for many games at once see
[Bulk setup](bulk-setup.md).

Analytics without double counting: the game's own SDKs keep reporting first purchases. In each integration
(admin page → the app → **Integrations**) turn on **Leave out first purchases of Google Play, App Store and Amazon**.
The server then sends only what the game cannot: renewals, refunds, cancellations, and purchases through Xsolla,
Stripe and Steam. To join them with the game's own events, the client sends its ids: Tenjin's
`analytics_installation_id` (`tenjin_id`) and Firebase's `app_instance_id` (sample **Attribution ids**).

## 2. The Unity package

The package does not need Unity IAP for this: the core works with Unity IAP 4, 5 or none.

```csharp
var ids = new PurserAnalyticsIds("<tenjin sdk key>");      // Samples~/AttributionIds
var billing = Billing.Create(config, new BillingDependencies
{
    Store = ExternalStoreAdapter.ForThisDevice(),           // google on Android, apple on iOS
    UserIdProvider = new LocalUserIdProvider(),              // or the game's player id
    AttributionProvider = ids,
    AnalyticsIdProvider = ids,
});
Billing.InitializeInBackground(billing);
```

Report every purchase where the game's IAP code handles it, before it finishes the purchase in the store (sample
**Observer mode** has both versions):

```csharp
// Unity IAP 4
public PurchaseProcessingResult ProcessPurchase(PurchaseEventArgs e)
{
    GrantReward(e.purchasedProduct);
    PurserObserver.Report(billing, e.purchasedProduct);   // ExternalPurchase.FromUnityReceipt + ReportPurchaseAsync
    return PurchaseProcessingResult.Complete;
}
```

In a shared wrapper this is one call in one place (for example the wrapper's `OnPurchasePending`), not a change in
every game.

Once, on the first launch with Purser, report what the player owns so the server follows it:
`await billing.ReportRestoredAsync(PurserObserver.Owned(storeController))`.

## 3. Modes

| | `ReportMode.RecordOnly` (default) | `ReportMode.VerifyFirst` |
|---|---|---|
| The game grants | right away, as before | after `ReportPurchaseAsync` answered `IsSuccess` |
| Server down | nothing changes for the player | after `BillingConfig.failOpenAfterSeconds`: `Success` with `Provisional`; 0 = wait |
| Forged or refunded | `PurchaseRejected` event: take it back if wanted | `Rejected` result: not granted |

Either way the purchase is kept on the device until the server answered (`purser.observed` in the storage), and is
verified again on the next launch, when the app returns to the foreground, or when a player id appears. Products with
`strictVerification` never get a provisional grant.

`failOpenAfterSeconds` also works in the full mode (Purser buys through Unity IAP 5): when the server does not answer
in time, the purchase is granted provisionally, finished in the store and verified later. Use it only with delivery
`none`: the server has to accept a purchase the client already finished.

## 4. What the server does with these purchases

`/verify` gets `client_finished: true`: the server checks the purchase with the store, records it, and does not
consume or acknowledge it (the game's IAP code did). Store notifications find the purchase by its token (Google) or
original transaction id (Apple), so renewals and refunds reach the player id the game reported, and the server sends
them to the integrations with the ids the client sent (kept for `retention_days`, longer while a subscription is
active).

## 5. Child-directed games

Set **Child-directed app** in the app's settings on the admin page (Families, Kids Category, COPPA): advertising ids
the client sends are dropped and never stored or sent on. In the Unity config, `childDirected` keeps them from being
sent at all.
