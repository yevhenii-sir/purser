# Observer mode

For games that keep their own Unity IAP code (Unity IAP 4 or 5) and add Purser next to it: the game buys, grants and
finishes purchases as before; Purser records each purchase, verifies it on the server (which then follows renewals,
cancellations and refunds and sends them to analytics) and keeps the customer info.

1. Billing server: a build per store with **delivery `none`** (the game grants).
2. Create the service with the observer store, once:

   ```csharp
   var billing = Billing.Create(config, new BillingDependencies
   {
       Store = ExternalStoreAdapter.ForThisDevice(),          // google on Android, apple on iOS
       UserIdProvider = new LocalUserIdProvider(),             // or the game's own player id
       AttributionProvider = new PurserAnalyticsIds(tenjinKey), // Samples: Attribution ids (Tenjin, Firebase)
       AnalyticsIdProvider = new PurserAnalyticsIds(tenjinKey),
   });
   Billing.InitializeInBackground(billing);
   ```

3. Report each purchase where the game's IAP code handles it, **before** it finishes the purchase:

   ```csharp
   // Unity IAP 4: IStoreListener
   public PurchaseProcessingResult ProcessPurchase(PurchaseEventArgs e)
   {
       GrantReward(e.purchasedProduct);                 // as before
       PurserObserver.Report(billing, e.purchasedProduct);
       return PurchaseProcessingResult.Complete;        // as before
   }

   // Unity IAP 5: StoreController
   void OnPurchasePending(PendingOrder order)
   {
       GrantReward(order);                              // as before
       PurserObserver.Report(billing, order);
       storeController.ConfirmPurchase(order);          // as before
   }
   ```

4. Once, on the first launch with Purser, report what the player already owns so the server follows it:
   `await billing.ReportRestoredAsync(PurserObserver.Owned(storeController));` (Unity IAP 5: the orders of
   `OnPurchasesFetched`).

5. Optional: `billing.PurchaseRejected += r => TakeBack(r.ProductId);` for purchases the server refused (forged
   receipts, refunds of a purchase it saw). With `ReportMode.VerifyFirst` the game waits for the answer before it
   grants; `BillingConfig.failOpenAfterSeconds` grants anyway when the server does not answer in time.

Purser keeps reported purchases on the device until the server answered, so a purchase made offline or while the
server was down is verified at the next launch.
