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
- `refund`, `expire`;
- `transfer` (below);
- `admin`: revoked by support from the admin page; handle it like `refund`;
- `replaced`: the player switched to another subscription (upgrade, downgrade, crossgrade in Google Play, an App Store subscription group or Stripe). The `grant` of the new product comes with its own `transaction_id`;
- `reinstated`: a purchase taken back is granted again: the merchant won a payment dispute (Stripe), or Apple reversed a refund (`REFUND_REVERSED`). A subscription period that ended meanwhile gets its revenue back without a `grant`;
- `paused` (`revoke`): the player paused a Google Play subscription; no access until it resumes. `/entitlements` lists it under `paused` with `resumes_at_ms`;
- `resumed`: never sent as a reason of its own: a paused subscription that runs again is granted with reason `renewal` or `purchase` like any reverified period.

`expires_at_ms` of a subscription is when access ends: the paid period, or the store's grace period after a failed renewal payment.

An event may arrive again on retries, so the game **must be idempotent**:
- **consumable** — grant once per `(transaction_id, event, reason)`: a `reinstated` grant follows a `revoke` of the same transaction;
- **non-consumables and subscriptions** — apply as state: “`user_id` has / doesn't have `product_id` until `expires_at_ms`”. Applying state is idempotent by itself. Don't drop repeats by `(transaction_id, event)` alone: after a transfer the same transaction legitimately arrives as a `grant` for a different `user_id`.

**Transfer (`transfer`).** Non-consumables and subscriptions belong to the store account. If a player restores them on a different `user_id` (reinstall, second profile), billing first sends the previous owner a `revoke` with `reason = transfer` and their `user_id`, and only after it is delivered successfully, a `grant` to the new one. The game must **remove the reward from the previous `user_id`**: otherwise one purchase would unlock several profiles. Until the `revoke` is delivered, the purchase is in `transfer_pending` status in the ledger (the previous owner is in `previous_user_id`), no `grant` is sent to the new owner, the worker retries the `revoke` with the usual backoff, and `/verify` and `/restore` respond `pending`. If a refund or subscription expiration arrives before delivery, the transfer is canceled and the previous owner receives the `revoke`. Transfers are not sent to GA4 or AppsFlyer. Consumables are never transferred (`ERR_TX_CLAIMED`). An app can limit transfers against account sharing (Delivery tab → Restores on another player: at most one per N days, at most N per purchase, or never); a refused restore answers `[ERR_TRANSFER_LIMIT]`, and support can still move a purchase on the player page.

- **`v1`** (recommended) — a single struct argument:

```rust
#[derive(spacetimedb::SpacetimeType)]
pub struct BillingEvent {
    pub event: String, pub reason: String, pub store: String, pub environment: String,
    pub app_id: String, pub product_id: String, pub kind: String,
    pub transaction_id: String, pub original_transaction_id: String, pub user_id: String,
    pub expires_at_ms: i64, pub price_micros: i64, pub currency: String, pub timestamp_ms: i64,
}

const BILLING_IDENTITY_HEX: &str = "<identity of the spacetime_token>";

#[spacetimedb::reducer]
pub fn billing_grant(ctx: &ReducerContext, ev: BillingEvent) -> Result<(), String> {
    if ctx.sender() != Identity::from_hex(BILLING_IDENTITY_HEX).unwrap() {
        return Err("unauthorized".into());
    }
    // if already applied (ev.transaction_id, ev.event) { return Ok(()) }
    // grant ev.product_id to ev.user_id, subscription until ev.expires_at_ms
    Ok(())
}
```

```csharp
[SpacetimeDB.Type]
public partial struct BillingEvent {
    public string @event; public string reason; public string store; public string environment;
    public string app_id; public string product_id; public string kind;
    public string transaction_id; public string original_transaction_id; public string user_id;
    public long expires_at_ms; public long price_micros; public string currency; public long timestamp_ms;
}

[SpacetimeDB.Reducer]
public static void billing_grant(ReducerContext ctx, BillingEvent ev) {
    if (ctx.Sender != Identity.FromHexString(BillingIdentityHex)) throw new Exception("unauthorized");
    // idempotent grant by ev.transaction_id
}
```

- **`webhook`** — `POST webhook_url` with the event as the body and the headers `X-Billing-Event` and `X-Billing-Signature: hex(HMAC-SHA256(webhook_secret, body))`. Verify the signature against the raw body. Any 2xx response counts as success. **Answer 2xx to `event` values you don't know and ignore them**: the setup check sends `{"event":"ping"}` (header `X-Billing-Event: ping`) from the admin panel's "Ping webhook" button.

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

`aud` (the app's key, `game_key`) is optional. Open builds take any player id: use ids nobody can guess (UUIDs), and personal attributes (`email`, `phone`, `name`) cannot be set there.

**Rate limits** (always on). Over a limit the answer is `429` with `"code": "ERR_RATE_LIMITED"` and `Retry-After: <seconds>`: wait that long and retry; a purchase is not finished because of it.

**Signed answers** (a response signing key). `/entitlements` and `/restore` answers carry `X-Purser-Signature`: the base64 Ed25519 signature of the exact response body. Check it with the build's public key (Delivery tab) before trusting or caching the answer, so an offline copy on the device cannot be edited.

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
