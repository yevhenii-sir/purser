# Paywalls, remote config and A/B tests

This page covers paywalls, placements, remote config and A/B tests. Read it when you want to change
what a shop or offer screen shows without shipping an app update.

The game asks the server what to show at a **placement** (`main_paywall`, `shop`, `level_failed_offer`).
The server answers with a **paywall**: an ordered list of products and a free-form JSON **remote config**.
In the admin page you change the products, their order and the config without an app update. You can
also run **A/B tests** between paywalls. This is the Adapty "placements / paywalls / A/B tests" model,
kept small.

## Model

| Thing | What it is | Stored in |
|---|---|---|
| Paywall | `paywall_id`, name, catalog product ids in display order, remote config (any JSON, up to 16 KB) | `paywall` (`{game_key}/{paywall_id}`) |
| Placement | `placement_id`, variants `[{variant_id, paywall_id, weight}]`, audiences `[{audience_id, name, paywall_id, rules}]`, enabled, the running or last test | `placement` (`{game_key}/{placement_id}`) |
| Test | test id, the variants and weights when it started, start / stop time, winner | `paywall_test` |
| Views | views and new viewers per day × placement × test × variant (one row, incremented) | `paywall_daily` |
| Viewers | The players who saw a placement: once per test, outside tests once a day. Kept for `retention_days` after the last view and erased with the player. | `paywall_viewer` |
| Purchases | the ledger row remembers `paywall_placement`, `paywall_variant_id`, `paywall_test_id`, `paywall_id` | `purchase` (new columns) |

Everything belongs to an **app** (a `game_key`). Its Android and iOS builds share paywalls and
placements.

A paywall lists catalog product ids from the Products tab. A catalog id is the product's own id, the
same in every store. Each build keeps the store's id next to it, for example a Stripe Price id or a
reverse-domain App Store id. Each build answers with the products configured for it, in the paywall's
order. It silently skips ids missing on that build, and the admin page marks them "not on iOS".

## What a player gets

The server picks the paywall in this order:

- **A player in an audience** (below): that audience's paywall, `variant_id` `audience.{audience_id}`,
  `test_id` empty. Audiences come before the variants and the test.
- **No running test:** the first variant of the placement, `test_id` empty.
- **Running test:** the variant `assign(test_id, user_id)`. It takes the first 8 bytes of
  SHA-256(`{test_id}:{user_id}`) as a big-endian integer, modulo the sum of the weights, and walks
  through the variants in order.
    - The same player always gets the same variant for a test, on every device and platform.
    - The server writes nothing per request.
    - A new test (new id) reshuffles players.
- **No `user_id`:** the first variant, outside the test. If you want the player in the test, ask for
  the paywall after the player id is known.
- **Disabled or unknown placement:** `404` with `code: "ERR_PLACEMENT_NOT_FOUND"`. The game shows its
  built-in fallback screen.

### Audiences

A placement can show other paywalls to groups of players. Examples: returning subscribers, players of
an old app version, a country, the game's big spenders.

- Audiences are checked in order, and the first one the player is in wins.
- A player in no audience gets the variants (or the test).
- Every rule left empty matches everyone.

| Rule | The player is in when |
|---|---|
| `platforms` | the build asked for is on one of them (`android`, `ios`, `web`, ...) |
| `min_app_version`, `max_app_version` | The client's `app_version` is within them, compared as dotted numbers (`1.10` > `1.9`). The version is the one sent with the request, else the last one the player's client reported. |
| `min_days`, `max_days` | the server first heard from the player in this build that many whole days ago |
| `subscriber` | `active`: has a subscription of the app that gives access now; `none`: has none |
| `owns_any`, `owns_none` | owns at least one / none of these catalog ids (non-consumables and running subscriptions) |
| `attributes` | each listed player attribute (`/attributes`) has one of the listed values, e.g. `{"country": ["DE", "AT"]}`, `{"segment": ["whale"]}` |

A rule needs the data it reads. A request without `user_id`, or a player the server never heard from,
is in no audience with a version, days or attribute rule.

Set country and spender status as attributes: they are what the game knows best.

Views of an audience's paywall count under its `variant_id`. Audiences are not part of A/B results.

### While a test runs

- Its variants and weights are frozen. Stop the test to change them.
- Its paywalls cannot be deleted.
- The paywalls' products and remote config can still be edited, which changes the test.

Stopping a test with a winner moves that variant first, so everyone gets it from then on. Test ids are
never reused.

## Results

The admin page shows these columns per variant (app → **Paywalls** → Results, or the
`admin_paywall_results` procedure):

| Column | Rule |
|---|---|
| Players | The players who saw the variant (`/paywall/event` with a `user_id`). A player counts once per test, however often they open it, and only for the variant the test assigns them. Outside tests, once a day. |
| Views | every view event (the client calls `LogPaywallShownAsync` once per display) |
| Buyers | the players with a first purchase (`reason = purchase`) whose `/verify` named this placement, test and variant |
| Purchases | those first purchases (a player who buys twice is one buyer, two purchases) |
| Renewals | Renewals of those subscriptions. A renewal row inherits the attribution of the purchase that started the subscription. |
| Revenue | Gross of purchases and renewals in USD, at the rate of the purchase day (ECB, market rates for the other currencies: the same rates as the revenue dashboard). Refunded rows (`revoke_pending` / `revoked`) are subtracted for net. |
| Conversion | buyers / players (shown as "—" without players) |
| ARPU / player | net revenue / players |
| Probability to be best | see below |

What the results leave out and cover:

- Manual grants and rejected rows are left out. Sandbox purchases are left out unless "Sandbox" is
  ticked.
- A test's results cover everything attributed to its id, whenever it happened. Clients that cached
  the answer can still report it shortly after the stop.
- "Outside tests" covers views and purchases with no test id in the last 30 days (`from` / `to` in the
  query).

### Probability to be best

**Probability to be best** is Bayesian. Each variant's conversion rate gets its exact
Beta(1 + buyers, 1 + players − buyers) posterior (uniform prior). Then
`P(i is best) = ∫ pdf_i(x) · Π_{j≠i} cdf_j(x) dx` is integrated numerically
(`purser_core::paywall::probability_best`).

- It is shown once every variant still shown has 100 players.
- Variants with weight 0 are not scored.
- It compares conversion, not revenue. A variant with cheaper products can win on conversion and lose
  on ARPU, so read both.

Wait for a few hundred players per variant, and a full week of traffic, before you pick a winner.

### Players, not views

The unit is the player. A player who opens the shop twenty times is one player. So a variant shown
more often (a paywall that pops up on every level) does not look worse per view than it is. Views stay
in the table for reference. Views without a `user_id` count as views only.

Purchases and views name their variant from the client. The server counts them for a test only when
the test assigns that variant to the player, so a modified client cannot move a test's result.

## HTTP API

Base: `https://<host>/v1/database/<your-billing-db>/route`.

### `POST /paywall`

```json
{"app_id": "com.example.mygame", "store": "google", "placement": "main_paywall", "user_id": "<player id, optional>"}
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

- `id` is the catalog product id (the Unity config's product `id`).
- `store_id` is the id in this build's store.
- They are the same unless the Products tab names another store id for the build.

Errors:

- `404 {"status": "not_found", "code": "ERR_PLACEMENT_NOT_FOUND" | "ERR_PAYWALL_NOT_FOUND", "message"}`
- `400` for an unknown app or a bad `user_id`.

### `POST /paywall/event`

```json
{"app_id": "com.example.mygame", "store": "google", "placement": "main_paywall", "variant_id": "b", "test_id": "main_paywall-20261110", "user_id": "...", "event": "view"}
```

The answer is `200 {"status": "ok"}`. Only `view` is supported.

The variant must be in the placement's current variants (no `test_id`) or in the test's variants.
Otherwise the answer is `400` with `ERR_UNKNOWN_VARIANT` / `ERR_UNKNOWN_TEST`.

### `/verify/google`, `/verify/apple`

Optional fields, copied from the `/paywall` answer the player bought from:

```json
{"placement": "main_paywall", "variant_id": "b", "test_id": "main_paywall-20261110", "paywall_id": "pw_sale"}
```

Invalid ids are dropped. The first attribution of a purchase wins: a replayed verify does not move it.

## Admin calls

| Call | Kind | Arguments |
|---|---|---|
| `admin_upsert_paywall` | reducer | `{"game_key", "paywall_id", "name", "product_ids": [...], "remote_config": {...}}` (or `remote_config_json` as text) |
| `admin_delete_paywall` | reducer | `game_key`, `paywall_id` (refused while a placement uses it) |
| `admin_upsert_placement` | reducer | `{"game_key", "placement_id", "variants": [{"variant_id", "paywall_id", "weight"}], "audiences": [{"audience_id", "name", "paywall_id", "rules": {...}}], "enabled"}` |
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

- **Products** must be in `BillingConfig`, because Unity IAP only fetches configured products. The
  server picks and orders among them. Server products the client does not know are listed in
  `Paywall.MissingProductIds`.
- **Cache:** the last answer per placement and player is cached in `IBillingStorage`. Offline,
  `GetPaywallAsync` returns it with `IsFromCache = true`. It throws `BillingException` when nothing is
  cached.
- **Remote config:** `RemoteConfigJson` (text), `RemoteConfig` (`JToken`), `RemoteConfigObject`
  (`JObject`), `GetRemoteConfig<T>()` and `GetValue(key, fallback)` read the config.
- **Editor:** with `editorMode = SkipServer`, `LogPaywallShownAsync` does nothing, so test sessions do
  not count as views. `GetPaywallAsync` still asks the server.
- **Attribution:** the paywall is sent with `/verify` for purchases started by
  `PurchaseAsync(productId, paywall)` in this session. A purchase finished after an app restart is
  verified without it.
