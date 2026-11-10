# Purser

Self-hosted in-app purchase billing for mobile games and apps, an open-source alternative to Adapty and RevenueCat. Google Play and App Store purchases are verified by your own [SpacetimeDB](https://spacetimedb.com) module, rewards are delivered straight into the game, and the admin panel shows the setup status and the revenue.

[Get started](getting-started.md){ .md-button .md-button--primary } [Self-host with Docker](self-hosting.md){ .md-button } [Admin panel demo](https://yevhenii-sir.github.io/purser/demo/){ .md-button }

```
┌──────────── client (Unity / GameMaker / …) ────────────┐
│  Unity: unity/com.yevhenii.purser (Unity IAP 5)  │
└──────────────┬─────────────────────────────────────────┘
               │ POST /verify, /restore, /entitlements
               ▼
┌──────── server/ — SpacetimeDB module (Rust) ───────────┐       Google Play Developer API
│  transaction ledger, consume/acknowledge, retries,     │◀────▶ App Store Server API
│  store notifications (RTDN, ASSN v2), revenue, GA4     │◀───── RTDN / Server Notifications
└──────────────┬─────────────────────────────────────────┘
               │ grant / revoke: SpacetimeDB reducer or HMAC-signed webhook
               ▼
          game database / backend
```

## Why

- **Self-hosted.** One SpacetimeDB module on maincloud or on your own server. No third-party service sits between the stores and your game.
- **No revenue share.** MIT-licensed and free; you pay only for your own hosting.
- **Your data.** The purchase ledger, players' entitlements and revenue live in your database.
- **Open source.** Read, audit and change how purchases are verified and rewards are delivered.

## Features

- **Server-side verification** of Google Play and App Store purchases, consume / acknowledge, restore, entitlements shared between the Android and iOS apps of a game.
- **A ledger with retries**: rewards are delivered exactly once even when the game server is down; refunds, renewals and expirations come from store notifications.
- **Admin panel** served by the module and organized by app: an *Add an app* wizard, a setup check with fix hints, products with Google Play import, integrations, keys, purchases, revenue, logs.
- **Revenue dashboard**: net revenue, refunds, subscriptions and MRR in USD (ECB rates), by app, product and currency. See [Revenue analytics](analytics.md).
- **Integrations**: GA4, AppsFlyer, Adjust, Tenjin and AppMetrica server-to-server events, a signed JSON webhook, raw event export to Google Cloud Storage (BigQuery), and Apple Ads campaign attribution in the Unity SDK.
- **Unity package** on Unity IAP 5 with VContainer / Zenject adapters; any other engine works over plain HTTP + JSON ([API reference](api.md)).

## Compared with Adapty and RevenueCat

Adapty and RevenueCat are hosted services with many more features; this project covers the core purchase backend and keeps it in your hands.

| | Purser | Adapty / RevenueCat |
|---|---|---|
| Hosting | Your SpacetimeDB database (maincloud or self-hosted) | Vendor cloud, managed for you |
| Cost | Free (MIT); your hosting costs | Free tier, then revenue share / paid tiers |
| Purchase data | In your database | On the vendor's servers, available through their dashboards, API and exports |
| Source code | Server and SDK are open source | Client SDKs are open source, the backend is not |
| Stores | Google Play, App Store | Google Play, App Store and more (for example web payments) |
| Paywall builder, A/B tests, cohorts | Not built in | Built in |
| Operations | You run and upgrade it | The vendor runs it |

## Where to go next

| | |
|---|---|
| [Getting started](getting-started.md) | Publish the module, open the admin panel, add an app, connect Unity |
| [Store setup](store-setup.md) | Google Play service account and RTDN, App Store keys and notifications |
| [Game integration](game-integration.md) | Delivery contract (reducers, webhook), games without a backend, other engines |
| [Self-hosting](self-hosting.md) | Your own SpacetimeDB server: Docker Compose, proxy allowlist, admin access |
| [API reference](api.md) | The public HTTP API (OpenAPI 3.1) |
| [MCP server](mcp.md) | Ask Claude (or any MCP client) about revenue, players, purchases and the setup |
| [Migrating from Adapty](migrating-from-adapty.md) | Running next to Adapty, legacy reducers, moving the ledger |
