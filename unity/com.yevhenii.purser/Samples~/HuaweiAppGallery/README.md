# Huawei AppGallery

Unity IAP has no Huawei store, so Huawei builds use `HuaweiStoreAdapter` over Huawei's In-App Purchases SDK.
This sample is the bridge to the [HMS Unity Plugin](https://github.com/EvilMindDevs/hms-unity-plugin) (`HMSIAPManager`).

1. Import the HMS Unity Plugin, enable In-App Purchases in its Kit Settings and put `agconnect-services.json` in place.
2. Add `PURSER_HMS_PLUGIN` to the Android scripting define symbols.
3. Create the billing service with the adapter on Huawei builds:

```csharp
var deps = new BillingDependencies { UserIdProvider = myUserIds };
if (BillingPlatforms.For(config) == BillingPlatform.Huawei)
    deps.Store = new HuaweiStoreAdapter(new HuaweiIapBridge());
var billing = Billing.Create(config, deps);
```

`BillingConfig.androidStore` decides when a build is a Huawei build: `Auto` (installed by AppGallery), or always
`HuaweiAppGallery` for a build made only for Huawei devices.

The bridge was written against `HMSIAPManager` of the plugin's 2.x versions and is not built by the package's CI:
check the member names against your plugin version. `HMSIAPManager.PurchaseProduct` reports nothing when the purchase
intent itself fails (no HMS account, IAP not activated); the adapter then reports the purchase as failed after 15 minutes.

On the billing server the Huawei build is its own build of the app (platform `huawei`) with `huawei_client_id` and
`huawei_client_secret`; add a `huawei` build with its app id to `BillingConfig.builds`. See docs/store-setup.md.
