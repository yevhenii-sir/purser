---
hide:
  - toc
---

# API reference

The billing module serves its HTTP API as SpacetimeDB routes of the billing database:

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
| `POST` | `/notify/google?secret=…` | Google Play RTDN (Pub/Sub push) |
| `POST` | `/notify/apple` | App Store Server Notifications V2 |
| `POST` | `/notify/amazon?app=…`, `/notify/xsolla?app=…`, `/notify/stripe?app=…` | Amazon RTN (SNS), Xsolla and Stripe webhooks |

Client routes may need a player token (`Authorization: Bearer`, builds with signed client requests), are rate limited (`429` + `Retry-After`) and, with a signing key, sign `/entitlements` and `/restore` answers (`X-Purser-Signature`): see [game integration](game-integration.md#client-requests-player-tokens-limits-signed-answers).

The machine-readable description is [openapi.yaml](openapi.yaml) (OpenAPI 3.1): generate a client from it or import it into Postman / Insomnia. Admin operations are not HTTP routes but SpacetimeDB reducers and procedures, documented in the [server reference](../server/README.md#configuration).

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
