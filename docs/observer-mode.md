# Observer mode: games with their own IAP code

This page shows how to add Purser to a game that already has its own purchase code. Read it if you want Purser's records and server-side events without replacing that code.

Most games already have purchase code: Unity IAP 4 or 5, a shared wrapper, analytics SDKs that report first purchases. Observer mode adds Purser next to that code without replacing it.

- The game keeps buying, granting and finishing purchases as before.
- Purser records each purchase, verifies it with the store and follows it afterwards.

What the game gets that it cannot see from the client:

- renewals, cancellations, billing problems, pauses and refunds of subscriptions (the stores tell the server);
- refunds of one-time purchases, so the game can take back what a refunded purchase gave;
- forged receipts refused by the server;
- revenue of all games in one place, and those events sent to Tenjin, GA4, AppMetrica, Adjust or AppsFlyer
  from the server.

## 1. The server

1. For each store build of the game, add a build with **delivery `none`**: the game grants, the server records.
2. Set up keys, products and store notifications as in [Store setup](store-setup.md). For many games at once, see
   [Bulk setup](bulk-setup.md).

### Analytics without double counting

The game's own SDKs keep reporting first purchases. To keep the server from counting them again:

1. Open the admin page → the app → **Integrations**.
2. In each integration, turn on **Leave out first purchases of Google Play, App Store and Amazon**.

The server then sends only what the game cannot: renewals, refunds, cancellations, and purchases through Xsolla,
Stripe and Steam.

To join them with the game's own events, have the client send its ids: Tenjin's `analytics_installation_id`
(`tenjin_id`) and Firebase's `app_instance_id`. The sample **Attribution ids** shows how.

## 2. The Unity package

The package does not need Unity IAP for this. The core works with Unity IAP 4, 5 or none.

First, create the billing service:

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

Then report every purchase where the game's IAP code handles it, before it finishes the purchase in the store. The
sample **Observer mode** has both versions.

```csharp
// Unity IAP 4
public PurchaseProcessingResult ProcessPurchase(PurchaseEventArgs e)
{
    GrantReward(e.purchasedProduct);
    PurserObserver.Report(billing, e.purchasedProduct);   // ExternalPurchase.FromUnityReceipt + ReportPurchaseAsync
    return PurchaseProcessingResult.Complete;
}
```

In a shared wrapper, this is one call in one place (for example the wrapper's `OnPurchasePending`), not a change in
every game.

Finally, on the first launch with Purser, report once what the player owns, so the server follows it:
`await billing.ReportRestoredAsync(PurserObserver.Owned(storeController))`.

## 3. Modes

The report mode decides when the game grants, and what happens when the server is down or refuses a purchase.

| | `ReportMode.RecordOnly` (default) | `ReportMode.VerifyFirst` |
|---|---|---|
| The game grants | right away, as before | after `ReportPurchaseAsync` answered `IsSuccess` |
| Server down | nothing changes for the player | after `BillingConfig.failOpenAfterSeconds`: `Success` with `Provisional`; 0 = wait |
| Forged or refunded | `PurchaseRejected` event: take it back if wanted | `Rejected` result: not granted |

In both modes the purchase is kept on the device until the server answered (`purser.observed` in the storage). It is
verified again:

- on the next launch;
- when the app returns to the foreground;
- when a player id appears.

Products with `strictVerification` never get a provisional grant.

`failOpenAfterSeconds` also works in the full mode, where Purser buys through Unity IAP 5. When the server does not
answer in time, the purchase is granted provisionally, finished in the store and verified later.

!!! warning
    Use `failOpenAfterSeconds` in the full mode only with delivery `none`. The server has to accept a purchase the
    client already finished.

## 4. What the server does with these purchases

`/verify` gets `client_finished: true`. The server checks the purchase with the store and records it. It does not
consume or acknowledge it, because the game's IAP code did.

Store notifications find the purchase by its token (Google) or original transaction id (Apple). So renewals and
refunds reach the player id the game reported.

The server sends them to the integrations with the ids the client sent. Those ids are kept for `retention_days`, and
longer while a subscription is active.

## 5. Child-directed games

Set **Child-directed app** in the app's settings on the admin page (Families, Kids Category, COPPA). Advertising ids
the client sends are then dropped and never stored or sent on.

In the Unity config, `childDirected` keeps them from being sent at all.
