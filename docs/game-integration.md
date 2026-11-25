# Game integration

How purchases reach the game: the delivery contract (SpacetimeDB reducers or a webhook), games without a backend, and what a client other than the Unity package has to do. The admin panel's **Add an app** wizard shows a ready reducer snippet with your billing identity, and the app's **Setup** tab verifies that the reducers exist and take the right arguments.

## Delivery events

Event (`v1` argument or webhook body; the webhook body also has `catalog_product_id`):

```json
{"event":"grant","reason":"purchase","store":"google","environment":"production","app_id":"com.game",
 "product_id":"month_sub","kind":"subscription","transaction_id":"GPA.1234..0","original_transaction_id":"GPA.1234",
 "user_id":"c200…","expires_at_ms":1793876523123,"price_micros":2990000,"currency":"USD","timestamp_ms":1791188212478}
```

`product_id` is the id in the store the purchase was made in. When the stores use different ids for one product (a Stripe Price id, a reverse-domain App Store id), webhook bodies also carry `catalog_product_id`, the app's own id of the product (the Products tab; the same as `product_id` when no other is set). The `v1` reducer argument keeps its fields, so grant by store id there (or keep the same id in every store). `/entitlements` is keyed by the catalog id.

`event`: `grant` | `revoke`; `reason`:
- `purchase`, `renewal`;
- `refund`, `expire`. A partial refund (part of a Stripe or Xsolla payment, some units of a multi-quantity Google Play purchase) sends no event: the purchase stays granted and only revenue changes. An App Store prorated refund revokes the purchase like any refund;
- `transfer` (below);
- `admin`: revoked by support from the admin page; handle it like `refund`;
- `replaced`: the player switched to another subscription (upgrade, downgrade, crossgrade in Google Play, an App Store subscription group or Stripe). The `grant` of the new product comes with its own `transaction_id`;
- `reinstated`: a purchase taken back is granted again: the merchant won a payment dispute (Stripe), or Apple reversed a refund (`REFUND_REVERSED`). A subscription period that ended meanwhile gets its revenue back without a `grant`;
- `paused` (`revoke`): the player paused a Google Play subscription; no access until it resumes. `/entitlements` lists it under `paused` with `resumes_at_ms`;
- `resumed`: never sent as a reason of its own: a paused subscription that runs again is granted with reason `renewal` or `purchase` like any reverified period.

`expires_at_ms` of a subscription is when access ends: the paid period, or the store's grace period after a failed renewal payment.

A Google Play subscription bought with add-ons (other subscription products billed with it) gives one `grant` per product: the add-ons come with `transaction_id` `{order}+{product}` and end with the subscription, or earlier when the player removes them.

An event may arrive again on retries, so the game **must be idempotent**: apply each **`event_id`** once. It is the same on every retry of an event and different for every other event of the purchase (a `reinstated` grant after a `revoke`, a second refund, a transfer to another `user_id`, a renewal with a new `expires_at_ms`). `occurred_at_ms` is when it happened (the same on every retry); `timestamp_ms` is when this attempt was sent.
- **consumable** — grant or take back the amount once per `event_id`, `quantity` times: a Google Play product with multi-quantity or a StoreKit purchase may carry several units (`price_micros` is what all of them cost; `quantity` is 1 otherwise);
- **non-consumables and subscriptions** — apply as state: “`user_id` has / doesn't have `product_id` until `expires_at_ms`”, and keep the latest `expires_at_ms` per player and product: a `grant` never shortens access (periods of a subscription are separate purchases, retried independently, so an older period's grant may arrive after a newer one's).

**Retries and ordering.** Delivery is at least once (`spacetimedb_idc`: exactly once per call). A call that gets no 2xx within 10 s, or any non-2xx answer (4xx too), is retried with backoff from 1 minute to 12 hours until it succeeds; the `delivery_failing` alert fires after 3 failed attempts. A request the game applied but answered too late arrives again: `event_id` makes that harmless. Events of one purchase go out one step at a time (a `revoke` waits until the `grant` in flight ended); events of different purchases are not ordered.

**Transfer (`transfer`).** Non-consumables and subscriptions belong to the store account. If a player restores them on a different `user_id` (reinstall, second profile), billing first sends the previous owner a `revoke` with `reason = transfer` and their `user_id`, and only after it is delivered successfully, a `grant` to the new one. The game must **remove the reward from the previous `user_id`**: otherwise one purchase would unlock several profiles. Until the `revoke` is delivered, the purchase is in `transfer_pending` status in the ledger (the previous owner is in `previous_user_id`), no `grant` is sent to the new owner, the worker retries the `revoke` with the usual backoff, and `/verify` and `/restore` respond `pending`. If a refund or subscription expiration arrives before delivery, the transfer is canceled and the previous owner receives the `revoke`. Transfers are not sent to GA4 or AppsFlyer. Consumables are never transferred (`ERR_TX_CLAIMED`). An app can limit transfers against account sharing (Delivery tab → Restores on another player: at most one per N days, at most N per purchase, or never); a refused restore answers `[ERR_TRANSFER_LIMIT]`, and support can still move a purchase on the player page.

- **`v1`** (recommended) — a single struct argument:

```rust
#[derive(spacetimedb::SpacetimeType)]
pub struct BillingEvent {
    pub event: String, pub reason: String, pub store: String, pub environment: String,
    pub app_id: String, pub product_id: String, pub kind: String, pub quantity: u32,
    pub transaction_id: String, pub original_transaction_id: String, pub user_id: String,
    pub expires_at_ms: i64, pub price_micros: i64, pub currency: String, pub timestamp_ms: i64,
    pub event_id: String, pub occurred_at_ms: i64,
}

const BILLING_IDENTITY_HEX: &str = "<identity of the spacetime_token>";

#[spacetimedb::reducer]
pub fn billing_grant(ctx: &ReducerContext, ev: BillingEvent) -> Result<(), String> {
    if ctx.sender() != Identity::from_hex(BILLING_IDENTITY_HEX).unwrap() {
        return Err("unauthorized".into());
    }
    // if already applied (ev.event_id) { return Ok(()) }
    // grant ev.product_id to ev.user_id, subscription until ev.expires_at_ms
    Ok(())
}
```

```csharp
[SpacetimeDB.Type]
public partial struct BillingEvent {
    public string @event; public string reason; public string store; public string environment;
    public string app_id; public string product_id; public string kind; public uint quantity;
    public string transaction_id; public string original_transaction_id; public string user_id;
    public long expires_at_ms; public long price_micros; public string currency; public long timestamp_ms;
    public string event_id; public long occurred_at_ms;
}

[SpacetimeDB.Reducer]
public static void billing_grant(ReducerContext ctx, BillingEvent ev) {
    if (ctx.Sender != Identity.FromHexString(BillingIdentityHex)) throw new Exception("unauthorized");
    // apply once per ev.event_id
}
```

- **`spacetimedb_idc`** — the game's SpacetimeDB database on the **same SpacetimeDB server or cluster** as billing (both on maincloud, or both on your server). Billing calls the game's reducers through [inter-database calls](https://spacetimedb.com/docs/databases/inter-database-communication): the call is written with the ledger step and SpacetimeDB delivers it once, in order, after a restart too, with billing's database as `ctx.sender()`. No token, no HTTP; the game database may be down for a while and gets the calls when it is up. The build's `spacetime_db` is the game database's **identity** (`spacetime list`). The game declares two reducers with exactly this signature (SpacetimeDB checks the parameter name, the struct's field names, types and order, and the `Result<(), String>` return; a call that does not match fails):

```rust
#[derive(spacetimedb::SpacetimeType)]
pub struct BillingEvent {
    pub event: String, pub reason: String, pub store: String, pub environment: String,
    pub app_id: String, pub product_id: String, pub kind: String, pub quantity: u32,
    pub transaction_id: String, pub original_transaction_id: String, pub user_id: String,
    pub expires_at_ms: i64, pub price_micros: i64, pub currency: String, pub timestamp_ms: i64,
    pub event_id: String, pub occurred_at_ms: i64, pub catalog_product_id: String,
}

#[spacetimedb::reducer]
pub fn purser_grant(ctx: &ReducerContext, event: BillingEvent) -> Result<(), String> {
    // accept ctx.sender() == billing's database identity only; apply once per event.event_id
    Ok(())
}

#[spacetimedb::reducer]
pub fn purser_revoke(ctx: &ReducerContext, event: BillingEvent) -> Result<(), String> { Ok(()) }
```

  `catalog_product_id` is always set here (the store id when the product has no catalog id). A reducer that returns `Err` rolls the game's transaction back; billing records the error and calls again with the usual backoff, so the reducers must be idempotent like every delivery. The reference module is [`server/game-receiver`](https://github.com/yevhenii-sir/purser/tree/main/server/game-receiver) (it also lets the owner allow billing's identity with `allow_purser`); the end-to-end check publishes it next to billing. The setup check shows billing's database identity for the game's allowlist. A TypeScript game module declares the same struct and reducers with `t.object` / `spacetimedb.reducer`; C# modules can receive these calls too (only declaring outboxes is limited to Rust and TypeScript).

- **`webhook`** — `POST webhook_url` with the event as the body and the headers `X-Billing-Event`, `X-Billing-Event-Id` (the `event_id`) and `X-Billing-Signature: t=<unix seconds>,v1=<hex(HMAC-SHA256(webhook_secret, "<t>." + body))>`. Verify against the raw body: recompute the HMAC of `t`, a dot and the body, compare in constant time, and refuse a `t` more than 5 minutes from your clock (a captured request cannot be replayed later; every retry is signed anew). While rotating `webhook_secret`, accept either secret for a while. Any 2xx response counts as success.

```python
import hashlib, hmac, time
def verify(header, body: bytes, secrets):
    parts = dict(p.split("=", 1) for p in header.split(","))
    t = int(parts["t"])
    if abs(time.time() - t) > 300:
        return False
    signed = f"{t}.".encode() + body
    return any(hmac.compare_digest(parts["v1"], hmac.new(s.encode(), signed, hashlib.sha256).hexdigest()) for s in secrets)
```

  Integration webhooks and owner alert webhooks are signed the same way. **Answer 2xx to `event` values you don't know and ignore them**: the setup check sends `{"event":"ping"}` (header `X-Billing-Event: ping`) from the admin panel's "Ping webhook" button.

### Games without a backend (`delivery = none`)

For simple games without a backend. Billing verifies the purchase and keeps the ledger, entitlements and analytics, but doesn't send the reward anywhere: the client grants it. For such apps the `/verify` response contains `"delivery": "client"` (otherwise `"server"`).

The order that guarantees the reward is never lost:
1. For Google consumables the server only **acknowledges**, so Google won't refund after 3 days. It does **not** consume.
2. On `success` the client grants the reward and saves `transaction_id` as already applied.
3. Only then does the client finish the purchase in the store: consume in Google, `finishTransaction` in Apple. The Unity package does this itself after the `PurchaseSucceeded` handlers.

If the game crashes between steps 1 and 3, the purchase isn't consumed and Google delivers it again. The server answers `success` from the ledger, and the client uses `transaction_id` to tell whether the reward was already granted. Non-consumables and subscriptions are checked via `/entitlements` (`IsActive` in Unity). The “consumed outside this server” protection doesn't get in the way here: the ledger row appears before the client consumes.

## Client requests: player tokens, limits, signed answers

Every client request is plain HTTP + JSON, whatever the engine. Three optional protections, set per build on the app's Delivery tab:

**Player tokens** (Client requests: *signed*). Without them anyone who knows a player id can read that player's `/entitlements` or change their attributes; with a game backend, turn them on. The backend signs a short-lived HS256 JWT for the signed-in player with the build's `client_auth_secret`, the client sends it as `Authorization: Bearer <token>` on every request, and requests without a valid one get `401` with `"code": "ERR_CLIENT_AUTH"` (ask the backend for a new token and retry).

```js
// Node (jsonwebtoken)
const token = jwt.sign({ sub: userId, aud: "<game key>" }, process.env.PURSER_CLIENT_AUTH_SECRET, { algorithm: "HS256", expiresIn: "1h" })
```

```csharp
// C# (System.IdentityModel.Tokens.Jwt)
var key = new SymmetricSecurityKey(Encoding.UTF8.GetBytes(secret));
var token = new JwtSecurityTokenHandler().WriteToken(new JwtSecurityToken(
    audience: "<game key>", claims: new[] { new Claim("sub", userId) },
    expires: DateTime.UtcNow.AddHours(1), signingCredentials: new SigningCredentials(key, SecurityAlgorithms.HmacSha256)));
```

`aud` (the app's key, `game_key`) is required: a token without it is refused (it would work for every app that shares the secret). Open builds take any player id: use ids nobody can guess (UUIDs), and personal attributes (`email`, `phone`, `name`) cannot be set there.

**Rate limits** (always on). Over a limit the answer is `429` with `"code": "ERR_RATE_LIMITED"` and `Retry-After: <seconds>`: wait that long and retry; a purchase is not finished because of it.

**Signed answers** (a response signing key). `/entitlements` and `/restore` answers carry `X-Purser-Signature`: the base64 Ed25519 signature of the exact response body. Check it with the build's public key (Delivery tab) before trusting or caching the answer, so an offline copy on the device cannot be edited.

## Your backend: server keys

A game with its own backend calls `/server/*` with a **server key** of the app (Delivery tab → Server keys → New key,
or `python seed.py --server-key <game_key> --key-scopes read,grant`; the key is made on your side and shown once, the
server keeps only its SHA-256). Send it as
`Authorization: Bearer psk_…`; it works for every build of the app and has scopes:

| Route | Scope | Body | What it does |
|---|---|---|---|
| `POST /server/entitlements` | `read` | `{"app_id", "user_id"}` | the player's customer info, the same as `/entitlements` |
| `POST /server/grant` | `grant` | `{"app_id", "user_id", "product_id", "transaction_id", "expires_at_ms"?, "note"?}` | gives a product without a store purchase (rewards, compensation): the game gets `grant` like for a purchase (store `manual`, no revenue). Idempotent by your `transaction_id`. |
| `POST /server/revoke` | `revoke` | `{"app_id", "purchase_id", "reason"?}` | takes a purchase back (`revoke`, reason `admin`); not a refund |

`401 ERR_SERVER_KEY`: no key, a malformed, unknown or revoked one. `403 ERR_SERVER_SCOPE`: the key lacks the scope or
the build belongs to another app. A key has its own rate limit (`429` + `Retry-After`). Every grant and revoke is in
the audit log under `server-key:<key_id>`. Never ship a server key in a client: use player tokens there.

## Client integration

**Unity:** a ready-made package [`unity/com.yevhenii.purser`](../unity/com.yevhenii.purser/README.md) on Unity IAP 5. It handles verification, order confirmation, restore, entitlements and binding purchases to the player.

For other clients (GameMaker, etc.):
1. Bind the purchase to the player using the same rule as the server:
   - Google: `obfuscatedAccountId` = `user_id` (if `user_id` is longer than 64 characters — SHA-256 hex);
   - Apple: `appAccountToken` = a UUID from the first 16 bytes of SHA-256(`user_id`) with version 8 / variant RFC 4122 bits.

   For consumables the server checks this value and won't credit the purchase to another account (`purser_core::account_binding_allows`). Non-consumables and subscriptions follow the store account: after a reinstall or on a new device, a player with a new `user_id` restores them via `/restore` and the purchase moves to them (it disappears from the previous `user_id`'s `/entitlements`, and the game receives a `revoke` with `reason = transfer` for it).
2. After a purchase → `POST /verify/{google|apple}`. On `success` / `pending` / `invalid` / `forbidden` finish the transaction in the SDK. On `error` or with no network, don't finish it — retry later. **Don't consume on the client in Google**: the server does it after granting the reward. Acknowledging on the client is fine. The exception is a `"delivery": "client"` response (`delivery = none`): then the client grants the reward itself and only then consumes.
3. On startup and on the “Restore purchases” button → `/restore` (or `/entitlements` if the SDK returned nothing).
4. The reward arrives in the game database; the client sees it through a SpacetimeDB subscription.

**GameMaker:** the Google Play Billing / Apple IAP extensions return `purchaseToken` / `transactionId` in the purchase async event. Then — `http_request(url, "POST", headers_map, json_stringify(body))`.

While the client still works through Adapty, Adapty acknowledges on Android itself. The server treats “already consumed/acknowledged” as success, so the transition period is safe. If Adapty or the client also **consumes** consumables, enable `client_consumes` for the app: otherwise such purchases will be rejected as granted elsewhere.
