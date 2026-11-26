# Samsung Galaxy Store

Unity IAP has no Samsung store, so Galaxy Store builds use `SamsungStoreAdapter` over Samsung's IAP SDK.
This sample is the bridge to Samsung's [IAP Unity plugin](https://developer.samsung.com/iap/samsung-iap-unity-plugin.html)
(`SamsungIAP.Instance`).

1. Import the Samsung IAP Unity plugin and add its `SamsungIAP` script to a GameObject of the first scene.
2. Add `PURSER_SAMSUNG_IAP` to the Android scripting define symbols.
3. Create the billing service with the adapter on Galaxy Store builds:

```csharp
var deps = new BillingDependencies { UserIdProvider = myUserIds };
if (BillingPlatforms.For(config) == BillingPlatform.Samsung)
    deps.Store = new SamsungStoreAdapter(new SamsungIapBridge(test: false));
var billing = Billing.Create(config, deps);
```

`BillingConfig.androidStore` decides when a build is a Galaxy Store build: `Auto` (installed by Galaxy Store), or
always `SamsungGalaxyStore` for a build made only for Galaxy Store.

The bridge was written against the plugin's 6.x API and is not built by the package's CI: Samsung documents the calls
(`GetProductsDetails`, `StartPayment`, `GetOwnedList`, `ConsumePurchasedItems`) but not the value classes, so check the
member names against your plugin version. `SamsungIapBridge(test: true)` uses `OPERATION_MODE_TEST` (licensed testers;
allow sandbox purchases on the server build).

On the billing server the Galaxy Store build is its own build of the app (platform `android`, store `samsung`) with
`samsung_service_account_id` and `samsung_private_key`; add a `samsung` build with its app id to
`BillingConfig.builds`. The server consumes consumables and acknowledges subscriptions; the adapter consumes granted
consumables too, for builds whose reward the client grants. See docs/store-setup.md.
