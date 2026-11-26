# Meta Quest

Unity IAP has no Meta Horizon Store, so Quest builds use `MetaQuestStoreAdapter` over the Meta XR Platform SDK
(`Oculus.Platform`). This sample is the bridge between the two.

1. Import the Meta XR Platform SDK and set the app's App ID. In the Developer Dashboard, complete the Data Use Checkup
   for **User ID** and **In-App Purchases** (without User ID the logged-in user's id is 0 and nothing can be verified).
2. Add `PURSER_META_PLATFORM` to the Android scripting define symbols.
3. Create the billing service with the adapter on Quest builds:

```csharp
var deps = new BillingDependencies { UserIdProvider = myUserIds };
deps.Store = new MetaQuestStoreAdapter(new MetaQuestPlatformBridge());
var billing = Billing.Create(config, deps);
```

The adapter sends the Meta user id and a new user proof (`Users.GetUserProof`, good for one check) with every
`/verify/meta` and `/restore` request; the server checks the proof with Meta before it looks the purchase up. The
server consumes consumables when it delivers the reward; when the game grants it (`delivery: none`) the adapter
consumes them after the server's answer. Do not consume purchases yourself before the server answered: Meta no
longer lists a consumed purchase, so the server could not verify it.

The bridge was written against the Platform SDK's Unity API and is not built by the package's CI: check the member
names against your SDK version. Older SDKs report a closed checkout as an empty purchase, newer ones as an error;
the bridge treats both as cancelled.

On the billing server the Quest build is its own build of the app (platform `android`, store `meta`) with `meta_app_id`
and `meta_app_secret`; add a `meta` build with its app id to `BillingConfig.builds`. See docs/store-setup.md.
