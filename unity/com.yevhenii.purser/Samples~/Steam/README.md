# Steam

Steam purchases are microtransactions started by the billing server, which keeps the publisher Web API
key. `SteamStoreAdapter` talks to the server (`/checkout/steam`, `/verify/steam`) and to Steam through
`ISteamMicroTxnBridge`; this sample is that bridge for [Steamworks.NET](https://github.com/rlabrecque/Steamworks.NET).

1. Install Steamworks.NET (20.2 or later) and initialize the Steam API (e.g. its `SteamManager` component).
2. Add `PURSER_STEAMWORKS_NET` to the Standalone scripting define symbols.
3. Create the billing service with the adapter on Steam builds:

```csharp
var bridge = new SteamworksNetBridge(() => SteamManager.Initialized);
var deps = new BillingDependencies { UserIdProvider = myUserIds, Store = new SteamStoreAdapter(config, bridge) };
var billing = Billing.Create(config, deps);
```

Add a `steam` build to `BillingConfig.builds` with the app id of the Steam build on the server (the Steam App ID). A purchase
opens the Steam overlay; when the player authorizes it, the server finalizes the order and `PurchaseSucceeded`
fires as for any store. Declining is reported as `Cancelled`.

The bridge also implements `ISteamWebApiTicketProvider`: every checkout carries a Steam Web API auth ticket
(`SteamUser.GetAuthTicketForWebApi("purser")`, hex-encoded), so the server can check the player's SteamID. Servers
whose Steam build sets `steam_require_ticket` refuse checkouts without it.

Other wrappers (Facepunch.Steamworks, your own) need the three values and the `MicroTxnAuthorizationResponse_t`
callback of `ISteamMicroTxnBridge`, and `ISteamWebApiTicketProvider` for the ticket. Server setup:
docs/store-setup.md#steam.
