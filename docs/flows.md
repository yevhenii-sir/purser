# How it works

Sequence diagrams of the main flows: who calls whom, in what order, and what the ledger row goes through. The details
of each step are on the pages linked under each diagram.

Names used below: **client** is the game on the player's device (the Unity package or any [SDK](sdks.md)), **Purser**
is the billing database, **game server** is your game's SpacetimeDB database or backend that receives rewards, and
the **ledger** is Purser's table of purchases (one row per store transaction).

## A purchase in Google Play or the App Store

The client buys through the store, then asks Purser to check the purchase. Purser looks it up in the store's API, writes
the ledger row first and only then calls anything else, so a failure at any later step is retried, never lost.

```mermaid
sequenceDiagram
    autonumber
    actor Player
    participant C as Client
    participant S as Store
    participant P as Purser
    participant G as Game server

    Player->>C: Buy
    C->>S: Purchase (account id derived from user_id)
    S-->>C: Purchase token / transaction id
    C->>P: POST /verify/google or /verify/apple
    P->>S: Look the purchase up (Play Developer API, App Store Server API)
    S-->>P: State, product, account id, price, expiry
    Note over P: Ledger row "verified"<br/>revenue counted, analytics events queued
    P->>G: grant (reducer, inter-database call or signed webhook)
    G-->>P: OK
    Note over P: "granted"
    opt Google Play
        P->>S: Consume (consumable) or acknowledge
    end
    Note over P: "completed"
    P-->>C: 200 success
    C->>S: Finish the order in the store SDK
    G-->>C: Reward arrives through the game's own data
```

- A consumable is credited only to the player whose id the purchase carries (`obfuscatedAccountId` / `appAccountToken`);
  a purchase consumed outside Purser is refused with `ERR_CONSUMED_ELSEWHERE`.
- If delivery does not finish within the request's time budget, `/verify` answers `202 pending` and the worker
  carries on (see [Delivery and retries](#delivery-and-retries)). The client finishes the order anyway: the ledger has it.
- On `error` or without network the client does **not** finish the order and asks again later.
- Amazon, Huawei, Samsung and Meta Quest work the same way with their own APIs.

More: [game integration](game-integration.md), [store setup](store-setup.md).

## Games without a backend (`delivery = none`)

Purser checks the purchase and keeps the ledger, entitlements and analytics; the client grants the reward itself.
The order below makes sure a crash at any point never loses a reward.

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant S as Store
    participant P as Purser

    C->>S: Purchase
    S-->>C: Purchase token
    C->>P: POST /verify/google
    P->>S: Look the purchase up
    opt Google Play
        P->>S: Acknowledge only (no consume)
    end
    P-->>C: 200 success, delivery "client"
    C->>C: Grant the reward, remember transaction_id
    C->>S: Finish (consume in Google, finishTransaction in Apple)
    Note over C,S: A crash before this step: the store delivers the purchase again,<br/>Purser answers success from the ledger, transaction_id tells it was granted
```

More: [games without a backend](game-integration.md#games-without-a-backend-delivery-none).

## Delivery and retries

Every outside call of a purchase (deliver to the game, consume in the store) is a step of its ledger row. The request
tries it at once; when it fails, the minute worker retries with backoff.

```mermaid
sequenceDiagram
    autonumber
    participant P as Purser (request)
    participant W as Purser worker
    participant G as Game server
    participant O as Owner

    P->>G: grant
    G--xP: Timeout (10 s) or non-2xx
    Note over P: Row keeps its step, next attempt in 1 min
    loop Backoff from 1 minute to 12 hours
        W->>G: grant (same event_id)
        G--xW: Still failing
    end
    W-)O: Alert "delivery_failing" after 3 failed attempts
    W->>G: grant (same event_id)
    G-->>W: 2xx
    Note over W: "granted", then the store step, then "completed"
```

- The game applies each `event_id` once: a call it applied but answered too late arrives again.
- With `spacetimedb_idc` delivery the call is written to an outbox in the same transaction and SpacetimeDB delivers it
  once; the ledger row waits for the game reducer's answer instead of retrying.
- A step that only a change of the setup can fix (a missing key, a host that is not allowed, a deleted build) is
  **blocked**: it waits until that setup is saved, or 6 hours.
- Events of one purchase go one at a time: a `revoke` waits for the `grant` in flight.

More: [retries and ordering](game-integration.md#delivery-events), [owner alerts](alerts.md).

## Renewals, refunds and expirations

Stores tell Purser about changes with notifications. A notification only says *what to look up again*: Purser asks the
store for the current state and moves the ledger from that, so a forged or replayed notification changes nothing.

```mermaid
sequenceDiagram
    autonumber
    participant S as Store
    participant P as Purser
    participant G as Game server
    participant I as Analytics

    S->>P: POST /notify/google (Pub/Sub push with a Google-signed token)<br/>or /notify/apple (signed payload, Apple certificate chain)
    P->>S: Look the purchase up again
    S-->>P: Current state
    alt Renewed
        Note over P: New ledger row for the period<br/>revenue counted
        P->>G: grant, reason "renewal", new expires_at_ms
    else Refunded or revoked
        Note over P: "revoke_pending"
        P->>G: revoke, reason "refund"
        Note over P: "revoked", refund counted
    else Expired
        P->>G: revoke, reason "expire"
    end
    P-)I: Event to every integration of the build
    P-->>S: 200
```

- Subscriptions are also re-checked by the worker on a schedule, so a renewal or expiration whose notification never
  arrived is still seen (Steam and Meta Quest have no notifications at all).
- Refunds that no notification reported are found by a sweep every 6 hours (Google Play voided purchases, App Store
  notification history).

More: [store setup](store-setup.md), [analytics](analytics.md).

## Restore on another player

Non-consumables and subscriptions belong to the store account. When a player restores them under a new `user_id`
(reinstall, second profile), the purchase moves: the previous owner loses it first.

```mermaid
sequenceDiagram
    autonumber
    participant C as Client (new user_id)
    participant P as Purser
    participant S as Store
    participant G as Game server

    C->>P: POST /restore {user_id, purchases}
    P->>S: Look each purchase up
    S-->>P: Owned
    Note over P: Owned by another user_id<br/>"transfer_pending"
    P-->>C: pending
    P->>G: revoke to the previous user_id, reason "transfer"
    G-->>P: OK
    P->>G: grant to the new user_id
    G-->>P: OK
    C->>P: POST /entitlements
    P-->>C: The product, active until expires_at_ms
```

An app can limit transfers (at most one per N days, at most N per purchase, or never); a refused restore answers
`ERR_TRANSFER_LIMIT`. Consumables never move (`ERR_TX_CLAIMED`).

More: [transfer](game-integration.md#delivery-events).

## Web checkout with Stripe

There is no client receipt on the web: Purser creates the Checkout Session, and Stripe's webhook is the source of
truth. Xsolla works the same way with its pay station.

```mermaid
sequenceDiagram
    autonumber
    actor Player
    participant C as Web client
    participant P as Purser
    participant St as Stripe
    participant G as Game server

    C->>P: POST /checkout/stripe {product_id, user_id, success_url}
    P->>St: Create Checkout Session (user_id in metadata)
    St-->>P: Session URL
    P-->>C: url
    C->>St: Redirect
    Player->>St: Pay
    St->>P: Webhook checkout.session.completed (signed)
    Note over P: Ledger row, revenue
    P->>G: grant
    St-->>C: Redirect to success_url with the session id
    loop Until paid
        C->>P: POST /verify/stripe {session_id}
        P-->>C: pending_or_canceled, then success
    end
```

Renewals, cancellations, refunds and disputes arrive as further webhooks. `/manage/stripe` opens Stripe's customer
portal for the player's subscriptions.

More: [Stripe](store-setup.md#stripe), [Xsolla](store-setup.md#xsolla).

## Steam

Steam purchases are driven by the server: Purser opens the order, the Steam overlay asks the player, and Purser
finalizes it.

```mermaid
sequenceDiagram
    autonumber
    actor Player
    participant C as Game (Steamworks)
    participant P as Purser
    participant St as Steam Web API

    C->>P: POST /checkout/steam {product_id, steam_id, ticket}
    P->>St: Check the auth ticket, GetUserInfo, InitTxn
    St-->>P: Order id
    P-->>C: order_id
    St->>Player: Overlay asks to authorize
    Player->>St: Authorize
    St-->>C: MicroTxnAuthorizationResponse_t
    C->>P: POST /verify/steam {order_id}
    P->>St: FinalizeTxn, QueryTxn
    Note over P: Ledger row, delivery to the game
    P-->>C: success
    opt The game closed before step 8
        P->>St: Worker: QueryTxn finds the authorized order and finishes it
    end
```

More: [Steam](store-setup.md#steam).

## Telegram Stars

Purser creates invoice links of the game's bot and is the bot's webhook.

```mermaid
sequenceDiagram
    autonumber
    participant C as Mini App
    participant P as Purser
    participant T as Telegram Bot API

    C->>P: POST /checkout/telegram {product_id, user_id}
    P->>T: createInvoiceLink
    P-->>C: url, order_id
    C->>T: Telegram.WebApp.openInvoice(url)
    T->>P: pre_checkout_query (webhook with the secret)
    P-->>T: answerPreCheckoutQuery ok (in the webhook's answer, within 10 s)
    T->>P: successful_payment
    Note over P: Ledger row, delivery to the game
    loop Until success
        C->>P: POST /checkout/telegram/status {order_id}
        P-->>C: pending_or_canceled, then success
    end
```

A `refunded_payment` update revokes the purchase.

More: [Telegram Stars](store-setup.md#telegram-stars).

## Analytics and attribution events

Every ledger transition queues one event per integration of the build in the same transaction. A sender works through
the queue outside the request, so a slow analytics service never slows a purchase.

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant P as Purser
    participant Q as Integration outbox
    participant I as AppsFlyer, GA4, Adjust, ...

    C->>P: POST /verify/... with device ids in attribution
    Note over P: Device ids remembered for the player<br/>(renewals and refunds have no client)
    P->>Q: One row per integration, same transaction as the ledger
    Q->>I: Send (payload built for that service)
    alt 2xx or a skip (sandbox, no device id)
        Note over Q: Done
    else Transient failure
        Q->>I: Retry with backoff
    else Gave up
        Note over Q: Kept as a dead event, can be replayed from the admin page
    end
```

More: [integrations](https://github.com/yevhenii-sir/purser/blob/main/server/README.md), [BigQuery export](bigquery.md).

## Paywalls and A/B tests

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant P as Purser
    participant S as Store

    C->>P: POST /paywall {placement, user_id}
    Note over P: Audience rules first, then the test's variant<br/>(the same player always gets the same one)
    P-->>C: paywall_id, variant_id, test_id, products, remote_config
    C->>P: POST /paywall/event {view, variant_id, test_id}
    C->>S: Purchase
    C->>P: POST /verify/... with placement, variant_id, test_id
    Note over P: The purchase is attributed to the variant<br/>(results on the Paywalls tab)
```

More: [paywalls and A/B tests](paywalls.md).

## Virtual currency

A product can give currency (`gems:100`). Purser keeps the balances; the client or the game's backend spends them.

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant B as Game backend
    participant P as Purser

    C->>P: POST /verify/... (a product that gives gems:100)
    Note over P: First grant of the purchase: +100 gems<br/>(a refund takes them back)
    P-->>C: success
    C->>P: POST /wallet
    P-->>C: balances {gems: 100}
    alt Spend from the client (builds with player tokens)
        C->>P: POST /wallet/spend {gems, 30, reference}
    else Spend from the backend (server key, scope wallet)
        B->>P: POST /server/wallet/spend {gems, 30, reference}
    end
    P-->>C: balance 70
    Note over P: Same reference again: nothing changes, repeated true
```

More: [virtual currency](wallet.md).
