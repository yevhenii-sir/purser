---
hide:
  - toc
---

# API reference

This page lists the public HTTP API: the routes that game clients, game backends, stores and monitors call. Use it when you integrate without an SDK or call Purser from a game backend.

The billing module serves its HTTP API as SpacetimeDB routes of the billing database. Every path below is relative to this base:

```
https://<host>/v1/database/<your-billing-db>/route
```

| Method | Path | Called by |
|---|---|---|
| `GET` | `/healthz` | monitoring (`503` when the worker stopped) |
| `POST` | `/verify/google`, `/verify/apple`, `/verify/amazon`, `/verify/steam`, `/verify/stripe` | game client after a purchase |
| `POST` | `/restore` | game client, "Restore purchases" |
| `POST` | `/entitlements` | game client / game backend: access levels, subscriptions, non-consumables |
| `POST` | `/paywall`, `/paywall/event` | game client: what to show at a placement, and that it was shown |
| `POST` | `/attributes` | game client: custom attributes of the player |
| `POST` | `/offers/apple/sign` | game client: an App Store promotional offer signature |
| `POST` | `/checkout/steam`, `/checkout/xsolla`, `/checkout/xsolla/status`, `/checkout/stripe` | game client: web and Steam purchases the server starts |
| `POST` | `/claim/xsolla` | game client: moves the Xsolla inventory of an old client-side login to the player |
| `POST` | `/manage/stripe` | game client with a player token: Stripe's customer portal for the player's subscription |
| `POST` | `/server/entitlements`, `/server/grant`, `/server/revoke` | game backend with a server key: read a player's purchases, give and take back products |
| `POST` | `/notify/google` | Google Play RTDN (Pub/Sub push with an OIDC token) |
| `POST` | `/notify/apple` | App Store Server Notifications V2 |
| `POST` | `/notify/amazon?app=…`, `/notify/xsolla?app=…`, `/notify/stripe?app=…` | Amazon RTN (SNS), Xsolla and Stripe webhooks |

Client routes follow these rules (see [game integration](game-integration.md#client-requests-player-tokens-limits-signed-answers)):

- **The build.** `app_id` is the app's id in its store (package name, bundle id). Builds in different stores may share it, so the routes whose path does not name a store (`/restore`, `/entitlements`, `/paywall`, `/paywall/event`, `/attributes`, `/redeem`, `/wallet`, `/wallet/spend`) also take `store`: the store of the build (`google`, `apple`, `amazon`, ...). The server finds the build by both.
- **Player tokens.** They may need a player token (`Authorization: Bearer`) in builds with signed client requests.
- **Rate limits.** They are rate limited (`429` + `Retry-After`).
- **Signed answers.** With a signing key, `/entitlements` and `/restore` answers are signed (`X-Purser-Signature`).

The machine-readable description is [openapi.yaml](openapi.yaml) (OpenAPI 3.1). Generate a client from it, or import it into Postman / Insomnia.

Admin operations are not HTTP routes. They are SpacetimeDB reducers and procedures, documented in the [server reference](../server/README.md#configuration).

<style>
  /* Redoc has its own light theme: keep it readable when the site is in dark mode. */
  #redoc-container { background: #fff; color: #333; border-radius: 4px; overflow: hidden; }
</style>
<div id="redoc-container"></div>
<script src="https://cdn.jsdelivr.net/npm/redoc@2.5.1/bundles/redoc.standalone.js"></script>
<script>
  Redoc.init("../openapi.yaml", {
    hideDownloadButton: false,
    expandResponses: "200,202",
    pathInMiddlePanel: true,
    theme: { typography: { fontFamily: "inherit", headings: { fontFamily: "inherit" } } }
  }, document.getElementById("redoc-container"));
</script>
