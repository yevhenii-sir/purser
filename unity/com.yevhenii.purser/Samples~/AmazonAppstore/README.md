# Amazon Appstore

Unity IAP 5 has no Amazon store, so Amazon builds use `AmazonStoreAdapter` over Amazon's own
Appstore SDK Unity plugin. This sample is the bridge between the two.

1. Import the Amazon Appstore SDK Unity plugin (Amazon Developer Portal → Appstore SDK).
2. Add `PURSER_AMAZON_APPSTORE_SDK` to the Android scripting define symbols.
3. Create the billing service with the adapter on Amazon builds:

```csharp
var deps = new BillingDependencies { UserIdProvider = myUserIds };
if (BillingPlatforms.For(config) == BillingPlatform.Amazon)
    deps.Store = new AmazonStoreAdapter(new AmazonAppstoreSdkBridge());
var billing = Billing.Create(config, deps);
```

`BillingConfig.androidStore` decides when a build is an Amazon build: `Auto` (installed by the Amazon
Appstore), or always `AmazonAppstore` for a build made only for Amazon.

On the billing server the Amazon build is its own build of the app (platform `amazon`) with the
`amazon_shared_secret` key; set `BillingConfig.amazonAppId` to its app id. See docs/store-setup.md.
