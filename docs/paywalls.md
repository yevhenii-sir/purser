# Paywalls, remote config and A/B tests

The game asks the server what to show at a **placement** (`main_paywall`, `shop`, `level_failed_offer`)
and gets a **paywall**: an ordered list of products and a free-form JSON **remote config**. You change
products, their order and the config in the admin page without an app update, and run **A/B tests**
between paywalls. This is the Adapty "placements / paywalls / A/B tests" model, kept small.

## Model

| Thing | What it is | Stored in |
|---|---|---|
| Paywall | `paywall_id`, name, catalog product ids in display order, remote config (any JSON, up to 16 KB) | `paywall` (`{game_key}/{paywall_id}`) |
| Placement | `placement_id`, variants `[{variant_id, paywall_id, weight}]`, enabled, the running or last test | `placement` (`{game_key}/{placement_id}`) |
| Test | test id, the variants and weights when it started, start / stop time, winner | `paywall_test` |
| Views | views and new viewers per day × placement × test × variant (one row, incremented) | `paywall_daily` |
| Viewers | the players who saw a placement: once per test, outside tests once a day; kept for `retention_days` after the last view, erased with the player | `paywall_viewer` |
| Purchases | the ledger row remembers `paywall_placement`, `paywall_variant_id`, `paywall_test_id`, `paywall_id` | `purchase` (new columns) |

Everything belongs to an **app** (a `game_key`): its Android and iOS builds share paywalls and
placements. A paywall lists catalog product ids (the Products tab): a product's own id, the same in
every store, while each build keeps the store's id next to it (a Stripe Price id, a reverse-domain App
Store id). Each build answers with the products configured for it, in the paywall's order, and silently skips ids missing on that build
(the admin page marks them "not on iOS").

## What a player gets

- **No running test:** the first variant of the placement, `test_id` empty.
- **Running test:** the variant `assign(test_id, user_id)`: the first 8 bytes of
  SHA-256(`{test_id}:{user_id}`) as a big-endian integer, modulo the sum of the weights, walked through
  the variants in order. The same player always gets the same variant for a test, on every device and
  platform, and the server writes nothing per request. A new test (new id) reshuffles players.
- **No `user_id`:** the first variant, outside the test. Ask for the paywall after the player id is
  known if you want the player in the test.
- **Disabled or unknown placement:** `404` with `code: "ERR_PLACEMENT_NOT_FOUND"`. The game shows its
  built-in fallback screen.

While a test runs its variants and weights are frozen (stop it to change them) and its paywalls cannot
be deleted; the paywalls' products and remote config can still be edited, which changes the test.
Stopping a test with a winner moves that variant first, so everyone gets it from then on. Test ids are
never reused.

## Results

The admin page (app → **Paywalls** → Results, or the `admin_paywall_results` procedure) shows per variant:

| Column | Rule |
|---|---|
| Players | the players who saw the variant (`/paywall/event` with a `user_id`): a player counts once per test, however often they open it, and only for the variant the test assigns them; outside tests once a day |
| Views | every view event (the client calls `LogPaywallShownAsync` once per display) |
| Buyers | the players with a first purchase (`reason = purchase`) whose `/verify` named this placement, test and variant |
| Purchases | those first purchases (a player who buys twice is one buyer, two purchases) |
| Renewals | renewals of those subscriptions: a renewal row inherits the attribution of the purchase that started the subscription |
| Revenue | gross of purchases and renewals in USD at the ECB rate of the purchase day (same rates as the revenue dashboard); refunded rows (`revoke_pending` / `revoked`) are subtracted for net |
| Conversion | buyers / players (shown as "—" without players) |
| ARPU / player | net revenue / players |
| Probability to be best | see below |

Manual grants, rejected rows and (unless "Sandbox" is ticked) sandbox purchases are left out. A test's
results cover everything attributed to its id, whenever it happened (clients that cached the answer can
still report it shortly after the stop). "Outside tests" covers views and purchases with no test id in
the last 30 days (`from` / `to` in the query).

**Probability to be best** is Bayesian: each variant's conversion rate gets its exact
Beta(1 + buyers, 1 + players − buyers) posterior (uniform prior), and
`P(i is best) = ∫ pdf_i(x) · Π_{j≠i} cdf_j(x) dx` is integrated numerically
(`purser_core::paywall::probability_best`). It is shown once every variant still shown has 100 players
(variants with weight 0 are not scored). It compares conversion, not revenue: a variant with cheaper
products can win on conversion and lose on ARPU, so read both. Wait for a few hundred players per
variant (and a full week of traffic) before you pick a winner.

The unit is the player: a player who opens the shop twenty times is one player, so a variant shown more
often (a paywall that pops up on every level) does not look worse per view than it is. Views stay in the
table for reference. Views without a `user_id` count as views only. Purchases and views name their
variant from the client; the server counts them for a test only when the test assigns that variant to
the player, so a modified client cannot move a test's result.

## HTTP API

Base: `https://<host>/v1/database/<your-billing-db>/route`.

### `POST /paywall`

```json
{"app_id": "com.example.mygame", "placement": "main_paywall", "user_id": "<player id, optional>"}
```

```json
{
  "status": "ok",
  "placement": "main_paywall",
  "paywall_id": "pw_sale",
  "paywall_name": "Sale 30%",
  "variant_id": "b",
  "test_id": "main_paywall-20261110",
  "products": [
    {"id": "premium", "store_id": "premium", "kind": "subscription"},
    {"id": "gems_100", "store_id": "gems_100", "kind": "consumable"}
  ],
  "remote_config": {"title": "Sale", "discount": 30},
  "updated_at_ms": 1794267768873
}
```

`id` is the catalog product id (the Unity config's product `id`) and `store_id` the id in this build's
store; they are the same unless the Products tab names another store id for the build.
Errors: `404 {"status": "not_found", "code": "ERR_PLACEMENT_NOT_FOUND" | "ERR_PAYWALL_NOT_FOUND", "message"}`,
`400` for an unknown app or a bad `user_id`.

### `POST /paywall/event`

```json
{"app_id": "com.example.mygame", "placement": "main_paywall", "variant_id": "b", "test_id": "main_paywall-20261110", "user_id": "...", "event": "view"}
```

`200 {"status": "ok"}`. The variant must be in the placement's current variants (no `test_id`) or in the
test's variants; otherwise `400` with `ERR_UNKNOWN_VARIANT` / `ERR_UNKNOWN_TEST`. Only `view` is supported.

### `/verify/google`, `/verify/apple`

Optional fields, copied from the `/paywall` answer the player bought from:

```json
{"placement": "main_paywall", "variant_id": "b", "test_id": "main_paywall-20261110", "paywall_id": "pw_sale"}
```

Invalid ids are dropped; the first attribution of a purchase wins (a replayed verify does not move it).

## Admin calls

| Call | Kind | Arguments |
|---|---|---|
| `admin_upsert_paywall` | reducer | `{"game_key", "paywall_id", "name", "product_ids": [...], "remote_config": {...}}` (or `remote_config_json` as text) |
| `admin_delete_paywall` | reducer | `game_key`, `paywall_id` (refused while a placement uses it) |
| `admin_upsert_placement` | reducer | `{"game_key", "placement_id", "variants": [{"variant_id", "paywall_id", "weight"}], "enabled"}` |
| `admin_delete_placement` | reducer | `game_key`, `placement_id` (refused while a test runs; history is kept) |
| `admin_start_paywall_test` | reducer | `game_key`, `placement_id`, `test_id` (empty: `{placement_id}-{yyyymmdd}`) |
| `admin_stop_paywall_test` | reducer | `game_key`, `placement_id`, `winner_variant_id` (empty: keep the order) |
| `admin_paywall_results` | procedure | `{"game_key", "placement_id", "test_id"}` or `{"game_key", "placement_id", "from", "to"}`, `include_sandbox` |
| `admin_preview_paywall` | procedure | `app_id`, `placement_id`, `user_id`: the `/paywall` answer, no view counted |

Errors carry codes: `ERR_BAD_PAYWALL`, `ERR_BAD_PLACEMENT`, `ERR_PAYWALL_IN_USE`, `ERR_TEST_RUNNING`,
`ERR_NO_TEST`, `ERR_BAD_TEST`, `ERR_PLACEMENT_NOT_FOUND`.

## Unity

```csharp
try
{
    var paywall = await billing.GetPaywallAsync("main_paywall");
    title.text = paywall.GetValue("title", "Go premium");
    foreach (var p in paywall.Products)                    // server order, BillingConfig products only
        AddButton(p.LocalizedPrice, () => billing.PurchaseAsync(p.Id, paywall));   // counts for the variant
    _ = billing.LogPaywallShownAsync(paywall);             // one view per display
}
catch (PaywallNotFoundException)
{
    ShowBuiltInPaywall();
}
```

- Products must be in `BillingConfig` (Unity IAP only fetches configured products): the server picks and
  orders among them. Server products the client does not know are listed in `Paywall.MissingProductIds`.
- The last answer per placement and player is cached in `IBillingStorage`; offline,
  `GetPaywallAsync` returns it with `IsFromCache = true`, and throws `BillingException` when nothing is cached.
- `RemoteConfigJson` (text), `RemoteConfig` (`JToken`), `RemoteConfigObject` (`JObject`),
  `GetRemoteConfig<T>()` and `GetValue(key, fallback)` read the config.
- In the Editor with `editorMode = SkipServer`, `LogPaywallShownAsync` does nothing, so test sessions do
  not count as views. `GetPaywallAsync` still asks the server.
- The paywall is sent with `/verify` for purchases started by `PurchaseAsync(productId, paywall)` in
  this session; a purchase finished after an app restart is verified without it.
