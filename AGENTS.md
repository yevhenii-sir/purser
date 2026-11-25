# AGENTS.md

Orientation for AI coding agents (and new contributors) working in this repository. Read this first,
then the file you are about to change. Human-facing docs: [README.md](README.md), [CONTRIBUTING.md](CONTRIBUTING.md),
[server/README.md](server/README.md) (API, tables, secrets reference) and [docs/](docs/).

## What this is

A self-hosted, open-source (MIT) in-app purchase backend, an alternative to Adapty / RevenueCat for
games and apps. It is one **SpacetimeDB module** (Rust → wasm) plus a **Unity package**:

1. The client buys through Unity IAP (or a store adapter: Amazon Appstore, Steam, web) and posts the
   receipt to `/verify/<store>` (`google`, `apple`, `amazon`, ...).
2. The module verifies it with the store's API (Google Play Developer API, App Store Server API,
   Amazon RVS), writes the **ledger**, consumes / acknowledges Google purchases.
3. It **delivers** the grant to the game: a reducer call on the game's own SpacetimeDB database, a
   signed webhook, or nothing (the client grants locally).
4. Store notifications (`/notify/<store>`: Google RTDN, Apple ASSN v2, Amazon RTN) drive renewals,
   refunds, expirations.
5. Every significant event goes to **integrations** (GA4, AppsFlyer, Adjust, Tenjin, AppMetrica,
   Amplitude, Mixpanel, Meta Conversions API, webhook, Google Cloud Storage export) and to **revenue analytics** (daily aggregates in USD).
6. An **admin page** (React, built into one HTML file embedded in the module) manages all of it.

## Repository map

| Path | What |
|---|---|
| `server/core` (`purser-core`) | Pure logic, no SpacetimeDB: the platform / store registry (`registry.rs`), store API URLs and parsers (`google.rs`, `apple.rs`, `amazon.rs`), SNS envelopes and signatures (`sns.rs`), JWT (`jwt.rs`), ledger model constants (`model.rs`), delivery contract (`event.rs`), integration payload builders (`integrations.rs`), setup-check rules (`doctor.rs`), catalog diff (`catalog.rs`), store product creation steps (`store_create.rs`), answers to Apple refund requests (`consumption.rs`), revenue rules (`revenue.rs`), FX (`fx.rs`), dates (`time.rs`), URL / form encodings, Basic auth and constant-time comparison (`encoding.rs`), amounts in micros (`money.rs`), X.509 / ECDSA chain checks for App Store signed payloads (`x509.rs`; the root is `apple_root_ca_g3.der`), Google Cloud setup requests (`google_setup.rs`), alert rules, throttle and messages (`alerts.rs`), paywall assignment and A/B results (`paywall.rs`). Native unit tests. |
| `server/spacetimedb` (`purser`) | The module: tables and reducers (`lib.rs`), HTTP routes (`handlers.rs`), product creation in the stores (`store_create.rs`, jobs run by a scheduled worker), one-click store setup: App Store notification URLs, Stripe webhook endpoint, Google Cloud with a one-time token (`store_setup.rs`), ledger state machine and the minute worker (`ledger.rs`), the worker's queue and the owned purchases derived from the ledger (`ledger_index.rs`), the queue drainers that work through it, `worker_lanes` at once (`drain.rs`), store clients, token cache and the dispatch by store (`stores.rs`), subscription re-checks (`poll.rs`), store notification health of every build (`store_health.rs`), secret resolution (`config.rs`), delivery (`delivery_client.rs`), integrations outbox and per-build health (`integrations.rs`), Apple refund request answers (`consumption.rs`), setup check (`doctor.rs`), health (`health.rs`), revenue (`revenue.rs`), paywalls, placements and A/B tests (`paywalls.rs`), catalog import (`catalog.rs`), settings (`settings.rs`), owner alerts and their outbox (`alerts.rs`), HTTP wrapper (`net.rs`; errors never show query strings), request plumbing: answers, body limit, query parameters (`web.rs`), `Ctx` trait and the time `Budget` of a request or worker run (`ctx.rs`), tests (`tests.rs`). |
| `server/spacetimedb/admin/index.html`, `admin/csp.txt` | **Generated** admin page and its Content-Security-Policy (script hash), both committed and embedded with `include_str!`. Never edit by hand. |
| `server/admin-ui` | Admin page sources: Vite, React 19, TypeScript, Tailwind v4, shadcn/ui, recharts, cmdk. |
| `server/seed.py`, `server/stdb.py`, `server/backup.py`, `server/publish_check.py`, `server/migrate.py`, `server/import_revenuecat.py`, `server/seed.example/`, `server/tests/` | CLI setup from JSON files, setup check from the terminal (`seed.py --check`), backups through the owner's `/sql` and the index-paged `admin_backup_page` (`backup.py backup|restore`, also for `--delete-data` releases), whether a module publishes in place (`publish_check.py`, SpacetimeDB's `pre_publish` plan), a copy of a database to another server (`migrate.py`), RevenueCat history import (`import_revenuecat.py`), unit tests of the scripts (`server/tests`). |
| `server/game-receiver` | Reference receiver of the `spacetimedb_idc` delivery (a SpacetimeDB module: `purser_grant` / `purser_revoke`), published by the end-to-end check. The module's receiver bindings in `spacetimedb/src/game_receiver/` are generated from it: `spacetime generate --lang rust --bindings module --bin-path <its wasm> --out-dir spacetimedb/src/game_receiver`; regenerate after changing it. |
| `server/setup_google.py` | Google Play setup from the terminal: gets a `gcloud` access token and calls the `admin_setup_google` procedure (APIs, service account + key, Pub/Sub topic, RTDN push subscriptions). |
| `server/mcp_server.py` | MCP server (stdlib JSON-RPC over stdio) over the admin procedures; see docs/mcp.md. |
| `unity/com.yevhenii.purser` | UPM package: `BillingService`, `BillingConfig`, providers, Editor inspector, EditMode tests, native iOS plugin (`Runtime/Plugins/iOS`). The core (`Purser.Runtime`) does not reference Unity IAP; the Unity IAP 5 adapter is the assembly `Purser.UnityIap` (`Runtime/UnityIap`, compiled only with `com.unity.purchasing` 5.4+ via the `PURSER_UNITY_IAP5` version define, registers `Billing.DefaultStoreFactory` on load), its tests are `Purser.Tests.UnityIap` (`Tests/Editor/UnityIap`). |
| `docs/` | store setup, game integration contract, analytics, self-hosting, migrating from Adapty. |
| `tools/generate_unity_meta.py` | Creates `.meta` files for new Unity package files (deterministic GUIDs). |
| `tools/e2e.py` | End-to-end check of a built module on a real SpacetimeDB server (CI runs it in a container). |
| `tools/loadtest.py` | Load test of a built module on a local SpacetimeDB, with a local proxy standing in for a slow store (not in CI). |
| `.github/workflows/` | `ci.yml` (tests, wasm, end to end on SpacetimeDB, admin page freshness, scripts, personal-infra grep guard), `release.yml` (tag `v*`), `audit.yml` (weekly `cargo deny` of the module's wasm dependencies, configured in `server/deny.toml`, and `npm audit`); `dependabot.yml`. |

## Domain model (names used everywhere)

- **App** (UI word) = a `game_key` group of builds. **Build** = one `game_app` row = one app in **one
  store** (`platform` = `android` | `ios` | `amazon` | `steam` | `web`; `store` column, empty = the
  platform's default; `web` names `stripe` or `xsolla`). `app_id` is the store's id where the store
  API needs it (Android package, iOS bundle) and any unique id otherwise (an Amazon build next to a
  Google Play build: `com.example.mygame.amazon`). "Game" in older code means the same as app.
- **Store registry** (`purser_core::registry`): every per-store fact (`StoreInfo`: platforms, secrets,
  client-receipt vs webhook verification, notification route and URL, finish step after the grant,
  refunds, price reporting, account binding, catalog import, subscription re-checks) and the
  platforms (`PlatformInfo`). Use `config::app_store(&app)` for a build's store; never compare
  `platform` with string literals. The admin page reads the snapshot `admin-ui/src/lib/registry.json`
  through `lib/stores.ts` (`storeOf`, `buildLabel`).
- **Developer account** (`game_app.account`): a secrets profile. Builds of one app may sit on
  different accounts (e.g. iOS published by a publisher).
- **Secrets** (`credential` table, `{scope}/{name}`) resolve **build → account → `default`**
  (`config::require_secret`). Names are in `config::secret_name` (+ `is_known`). Never return
  secret values from any API; the admin page only learns which keys exist.
- **Product** (`{app_id}:{product_id}`, `product_id` = the store's id): kind `consumable` | `non_consumable` |
  `subscription`. `catalog_id` (`Product::catalog_id()`, empty = `product_id`) is the app's own id, the same in every
  store (the Unity config's product `id`): paywalls list it, entitlements and webhook events carry it, the admin page
  groups by it. The ledger, store calls and revenue use the store id.
- **Purchase / ledger** id `{store}:{transaction_id}`; `purchase_token` is what the store needs to look
  the row up again (Google purchase token, Amazon user id). States (`purser_core::model::status`):
  `verified` → `granted` → `completed` (consumed / acknowledged; stores without a server-side finish
  go `verified` → `completed`); `revoke_pending` → `revoked`; `expire_pending` → `expired`;
  `transfer_pending` (restore moved it to another user); `rejected`. Every outside call is retryable:
  the row is written first, `ledger::advance` tries inline, `ledger::process_pending` (scheduled
  worker) retries with backoff.
- **Delivery** (`game_app.delivery`): `spacetimedb` (call `grant_reducer` / `revoke_reducer` on the
  game DB with `spacetime_token`; one argument, the `BillingEvent`), `spacetimedb_idc` (a game DB on the
  same server: outbox rows to its `purser_grant` / `purser_revoke`, `spacetime_db` = its identity; the
  ledger row waits in `idc_call` for the answer, `idc.rs`), `webhook` (HMAC `webhook_secret`),
  `none` (client grants; `PurchaseResult.GrantOnClient`). Contract: [docs/game-integration.md](docs/game-integration.md).
- **Account binding**: Google `obfuscatedAccountId` / Apple `appAccountToken` derived from `user_id`
  (`purser_core::google_account_id` / `apple_account_token`, mirrored by Unity `AccountIds` with the
  same test vectors). Enforced for consumables only.
- **Integrations**: `integration` row per build and kind (`{app_id}/{kind}`, `settings_json`) +
  a secret. Ledger transitions enqueue `integration_outbox` rows in the same transaction;
  `send_integration_event` builds the call with a pure builder from `purser_core::integrations` and
  retries transient failures (`is_retryable_status`). Client device ids arrive in `attribution`
  (`attribution_key::ALL`, sanitized) and are remembered in `user_attribution` for events without a
  client (renewals, refunds).
- **Revenue**: `revenue_daily` (day × app × product × environment × currency), `revenue_country_daily`
  (day × app × billing country × environment × currency), `subscription_daily`
  snapshots, ECB rates (`fx_day`, `fx_override`). Rules in `purser_core::revenue` are used both live
  and by `admin_rebuild_revenue`, so both give the same numbers.

## SpacetimeDB facts that bite (version 2.16.3, `features = ["unstable"]`)

- All tables are **private**. Only the database owner can read them with `/sql`, and owner-only DML
  (`INSERT INTO admin ...`) is how the first admin is added when `init` did not run (database created
  in the maincloud dashboard). Non-owners get "no such table".
- HTTP handlers (`handlers.rs`) get a **random `sender()` per request**: they cannot authenticate the
  caller. Everything admin is a reducer or `#[procedure]` that calls `access::require(ctx, Cap::…)` (in procedures
  inside `ctx.tx`). Admin procedures return a JSON **string** (`{"error": "..."}` on failure); `/call`
  wraps it in a JSON string literal, the UI's `proc()` unwraps both layers.
- `/call/<name>` looks for a reducer first: every **procedure** call logs
  `External attempt to call nonexistent reducer "<name>"`. Harmless; the Logs page hides it.
- `/sql` has no `SUM` / `GROUP BY`: aggregates are maintained by the module.
- **`init` does not run on republish** and there is no upgrade hook (see Schema rules).
- Outbound HTTP: only from procedures and HTTP handlers, outside a transaction; the procedure waits
  for the reply. `net::send` sets a 10 s timeout (`net::TIMEOUT_SECS`; the host allows up to 180 s and
  uses 30 s by default: the 500 ms in the `spacetimedb` crate docs is outdated). Private addresses
  are blocked. Keep calls idempotent (a timeout on our side may still have reached the store) and
  check `ctx::Budget::left()` before starting more work in a loop: a request spends 3 s on calls, a
  worker run or queue drainer 40 s, and a drainer continues the rest. `Budget::bound_calls` cuts every
  call at the deadline (`net::send` reads it); requests, `ledger::run_within` and the worker set it.
- HTTP routes and procedures run on a pool of module instances (one per CPU thread by default,
  `[wasm] procedure-instance-pool-size` in the server's `config.toml`; not configurable on maincloud);
  reducers run on one. A call waiting for a store keeps its instance, and every route shares the pool:
  purchases per second ≈ pool ÷ the store's latency, and a slow store slows `/entitlements` too. Keep
  waits short and bounded (the request deadline, `worker_lanes` drainers). Delivery to self-hosted SpacetimeDB hosts needs them in the `spacetime_host` allowlist.
- The module RNG is seeded by the timestamp: never use it for secrets.
- Maincloud tokens have no `hex_identity` claim; the UI asks the `whoami` procedure.
- Open transactions through `Ctx::tx` (see `ctx.rs`), not `with_tx` directly: it records the time the
  `Budget` reads, and tests count transactions through it.
- Write ledger rows only through `ledger::insert_purchase` / `ledger::save_purchase`: they keep the
  `purchase_count` table (the Overview counters) and the derived tables of `ledger_index.rs` in step:
  `pending_purchase` (the worker's queue: rows with `next_attempt_at_micros > 0`) and
  `owned_purchase` (what players own now). Hot paths read those, never ledger history by
  `user_id` or by due time. Rows written with SQL need `admin_reindex_ledger`.
- Admin reads must not scan big tables per request: read through an index (`app_id`, the
  `(app_id, created_at)` / `(app_id, updated_at)` ledger indexes, `(app_id, day)` of the daily
  revenue tables) and keep per-request work independent of the number of apps.
- Tiered storage: a table's `cache_policy` says where its rows may live. Ledger history (`purchase_log`,
  `admin_audit`, the daily aggregates) is `no_requirement`, `purchase` and player data `required_on_disk`,
  everything requests and the worker read every time `required_in_memory`. Reading a row that is not
  in memory can fail and roll the transaction back; the ledger retries, but don't add full scans of
  the cold tables outside the revenue rebuild and backups (they pull every row back into memory).

## Schema rules (no users yet: no legacy)

- Purser has no production users yet: change tables the clean way (rename, reorder, retype, remove)
  instead of keeping legacy columns, `#[default]` attributes or upgrade code. A release whose schema
  cannot be migrated in place says in both changelogs that it needs a publish with `--delete-data`.
- New tables and indexes migrate in place (SpacetimeDB adds indexes automatically).
- Every table sets `cache_policy` explicitly (`every_table_declares_a_cache_policy`): the default
  becomes `no_requirement` in SpacetimeDB 3.0. Changing a policy migrates in place.

## Client API rules (the server and the Unity SDK ship together)

- Request fields have one name each (`app_id`, never aliases); optional fields are only those a
  client may legitimately leave out, not ones kept for older clients.
- `server/spacetimedb/sdk-requests/current.json` holds what the current Unity SDK sends;
  `every_request_of_the_unity_sdk_is_accepted` parses it and `the_unity_sdk_purchase_flow_works`
  replays the purchase flow. Change it together with `Runtime/Api/Dto.cs` and the store adapters.

## Commands

```bash
cd server
cargo test -p purser-core --locked
cargo test -p purser --locked
cargo build -p purser --target wasm32-unknown-unknown --release --locked

cd server/admin-ui
npm ci
npm run build      # writes ../spacetimedb/admin/index.html and csp.txt — commit both (CI checks they are fresh)
npm run dev        # proxies to a local `spacetime start` on :3000 (SPACETIME_URL to change)

python tools/generate_unity_meta.py          # after adding files to the Unity package
python -m py_compile server/*.py tools/*.py
python -m unittest discover -s server/tests   # unit tests of the scripts
python tools/loadtest.py --help               # load test on a local SpacetimeDB (see its docstring)
```

Unity package tests in a real editor (CI does not run them): a local project in `unity/TestProject/` (gitignored)
with `Packages/manifest.json` = `{"dependencies": {"com.yevhenii.purser": "file:../../com.yevhenii.purser",
"com.unity.purchasing": "5.4.4", "com.unity.test-framework": "1.1.33"}, "testables": ["com.yevhenii.purser"]}` and `ProjectSettings/ProjectVersion.txt`
naming the oldest supported editor (`m_EditorVersion: 2022.3.62f3`); Unity 6 gets projects of its own
(`unity/TestProject6.0`, `6.3`, `6.6`: opening a project in a newer editor upgrades it) with the test framework the
editor bundles (`1.6.0` on 6.0 / 6.3, `1.8.0` on 6.6). `unity/TestProjectNoIap` (2022.3, no `com.unity.purchasing`) checks
that the core compiles and its tests pass without Unity IAP. `unity/TestProjectIap4` (2022.3, `com.unity.purchasing` 4.13.0) checks the core with Unity IAP 4
and the **Observer mode** sample (copy `Samples~/ObserverMode` into its `Assets/`). Then:

```bash
"<Unity 2022.3>/Editor/Unity.exe" -batchmode -nographics -projectPath unity/TestProject -runTests \
    -testPlatform EditMode -testResults results.xml -logFile unity.log
```

Unity runs awaits on its editor synchronization context: a continuation that waits for something completed later (a
cancellation) runs on the next editor update, after a synchronous test has looked. Such tests use
`Sync.InlineContinuations()` (`Tests/Editor/Fakes.cs`).

Build order matters: `npm run build` **before** the wasm build, because the page is embedded.
Don't run a full `cargo update` (keep the crates SpacetimeDB is built with); upgrade SpacetimeDB
with `cargo update -p spacetimedb --precise <version>`.

Local run with the admin page:

```bash
spacetime start --in-memory -l 127.0.0.1:3013
# create a throwaway identity and publish as it, so it owns the DB and the page works with its token:
curl -X POST http://127.0.0.1:3013/v1/identity                      # → {identity, token}
curl -X PUT http://127.0.0.1:3013/v1/database/<name> -H "Authorization: Bearer <token>" \
     -H "Content-Type: application/wasm" --data-binary @target/wasm32-unknown-unknown/release/purser.wasm
# open http://127.0.0.1:3013/v1/database/<name>/route/admin and sign in with that token
```

Run `spacetime` CLI commands from outside `server/` (or pass the database explicitly): a local
`server/spacetime.json` (gitignored) may point the CLI at someone's real database.

## Conventions

- **Logic goes to `core` as pure functions with tests**; the module does storage and HTTP only.
  Module tests (`tests.rs`) use `World` + `Fake` HTTP (`w.script(url_substring, status, body)`,
  `w.set_secret`, `w.as_admin`, `w.ctx()`); add one for every new behavior that touches tables.
- Errors returned to clients/admins carry a code prefix: `[ERR_ADMIN_UNAUTHORIZED]`, `[ERR_BAD_JSON]`, ...
- Settings and secrets are separate: non-secret config in rows (`settings_json`), keys in `credential`.
- Admin UI: hash routes in `routes.tsx` (`#overview`, `#apps`, `#app/<key>/<tab>`, `#accounts`,
  `#server`, `#logs`, `#admins`, `#player/<user_id>`); data via `sql()` / `call()` / `proc()` in `lib/api.ts`; shared
  pieces in `components/common.tsx` (`PageHeader`, `Section`, `DataTable`, `Field`, `DialogBody`,
  `Tone`, ...). Larger lists use `RichTable` (`components/rich-table.tsx`: the same columns plus `sort`,
  `hidden`, `size`; sorting, paging, resizable and hideable columns) on the ReUI data grid in
  `components/reui` (MIT, its LICENSE.md is kept there; add ReUI parts with `npx shadcn@latest add @reui/<name>`).
  Add shadcn components with `npx shadcn@latest add <name>`. Strict CSP: no external
  scripts, styles, fonts or requests. Use `DialogBody` for scrollable dialog content; give flex
  children that must keep their size `shrink-0` and text `min-w-0` / `truncate`.
- Admin page demo (`npm run build:demo` → `dist-demo/index.html`, published by the Docs workflow at
  `/demo/`): `src/lib/demo.ts` answers `sql()` / `call()` / `route()` from `demo/snapshot.json` and
  refuses writes; `__DEMO__` is a build constant, so none of it reaches the module's page. When an admin
  procedure or table the page reads changes, regenerate the snapshot with `tools/demo_snapshot.py`
  (see `server/admin-ui/demo/README.md`) and handle new procedures in `demo.ts`.
- Unity: no DI dependency in the core (`IBillingService`, `BillingService`); optional adapters in
  `Integrations/`. New files need `.meta` files (run the generator). Keep the public API documented
  with XML comments. Native code goes to `Runtime/Plugins/<platform>` with a `PluginImporter` meta.
- Docs and code are in **English**. Keep `CHANGELOG.md` (repo) and the Unity package `CHANGELOG.md`
  updated under "Unreleased".
- **No personal infrastructure** in the repo: use placeholders (`<your-billing-db>`,
  `com.example.mygame`, `stdb.example.com`). CI greps for known private hostnames and fails.
- Never commit secrets (`server/secrets/`, `*.p8`, service-account JSON are gitignored).

## Recipes

### Add a store

A store is a module of its own plus one line in the registry. Implemented: Google Play, App Store,
Amazon Appstore (`amazon.rs`, client receipts + SNS notifications), Steam (`steam.rs`, server-driven
checkout + worker polling), Stripe and Xsolla (`stripe.rs`, `xsolla.rs`, web checkout + signed
webhooks). Copy the closest one. A store that is declared but not built yet is a `planned(...)` line
in `registry::STORES`; builds cannot use it.

1. **Core** `server/core/src/<store>.rs` (+ `pub mod` in `lib.rs`): `pub const STORE: StoreInfo` (copy
   `amazon::STORE`; `implemented: true`, `verification`, `notifications` + `notify_query` /
   `notify_secret` / `notify_label`, `secrets` with `need` / `account_wide` / `file`, `finish`,
   `reports_price`, `binding`, `app_id_*`, `docs` = the anchor of its section in
   docs/store-setup.md), URL builders that sanitize / percent-encode every client value, and pure
   parsers to `StoreTx` (`store: store::<ID>`, `transaction_id` unique per store, `purchase_token` =
   whatever a later look-up needs, `environment` sandbox for test purchases, `state`,
   `expires_at_ms`, price when the store reports one). Webhook stores: signature verification and
   event parsing here too (see `sns.rs`). Unit tests next to it. A new store fills the existing
   `StoreTx` fields; add one only for a fact the ledger keeps for every store (as `paused_until_ms`,
   `family_shared`, `offer_type` were).
2. **Registry**: add `&crate::<store>::STORE` to `registry::STORES` (or replace its `planned(...)` line)
   (one line; the blank lines around it keep parallel additions mergeable). A new store id also
   needs a `model::store` constant and the platform's `stores` list in `PLATFORMS`.
   Then `UPDATE_REGISTRY_JSON=1 cargo test -p purser-core registry_snapshot` and commit
   `admin-ui/src/lib/registry.json` (the test fails until you do).
3. **Secrets**: nothing to add in `config::secret_name::is_known` (store secrets come from the
   registry); add a constant to `secret_name` if module code reads the secret.
4. **Module store client** `server/spacetimedb/src/stores.rs`: `<store>_fetch(ctx, app, product, ...)`
   (one HTTP call per step, `net::send` timeout, map statuses to `StoreError::{Invalid, Config, Transient}`,
   never put secrets into error texts), then one arm each in `stores::fetch` (client receipts),
   `stores::missing_field` (required request fields; retryable `error`, not `invalid`) and, if the
   server finishes purchases in the store, `stores::finish`.
5. **Routes** `server/spacetimedb/src/handlers.rs`: `/verify/<store>` and `/notify/<store>` are
   registered from the registry (`verify_route` / `notify_route`); add a `handle_<store>_notification`
   and its arm in `dispatch_notification`. Notifications only say what to look up again
   (`stores::refetch` → `sync_store_tx`); keep health in `store_health::touch`. The test
   `every_registry_route_has_a_handler` fails until the arm exists. New client fields go to
   `VerifyRequest` / `RestoreItem` (and `sdk-requests/current.json` with the SDK).
6. **Setup check** `server/spacetimedb/src/doctor.rs`: an online check arm in `check_app`
   (`check_<store>`, classifier in `purser_core::doctor`); notifications are reported generically
   from `store_notify_health`.
7. **Ledger**: usually nothing. `consumed_elsewhere`, `find_related`, `find_owner` have Google-only
   branches; give the store its own only if it has such state. Set `polls_subscriptions` when
   renewals may not be notified (the worker re-checks via `stores::refetch`).
8. **Admin page**: nothing store-specific beyond the registry snapshot; check the wizard and Keys
   tab with a build of the new store (`npm run build`).
9. **Creating products** (optional): if the store has a catalog API, list the kinds in
   `product_create` and add its steps to `purser_core::store_create` (`request` / `absorb`, each step
   safe to repeat) and its credentials to `auth` in the module's `store_create.rs`.
10. **Unity**: a `BillingPlatform` value exists for Amazon, Steam and Web. Implement `IStoreAdapter`
   (and `IStoreNameProvider` when the platform does not imply the store) in `Runtime/Store/`, map
   the purchase to `StoreOrder` (`TransactionId`, `PurchaseToken`, `StoreUserId`) and cover it with
   a fake in `Tests/Editor`. Steam / web SDKs are not dependencies of the package: put SDK glue in a
   sample guarded by a scripting define (see `Samples~/AmazonAppstore`).
11. **Tests and docs**: module tests in `tests.rs` (`w.script(url_substring, status, body)` for the
    store API, a build created through `admin_upsert_game_app`), a section in docs/store-setup.md,
    the stores table and HTTP API in server/README.md, both changelogs.

### Add an integration (outbound events)

1. `server/core/src/integrations.rs`: a constant in `integration_kind` (+ `ALL`), a `mod <kind>`
   with `Settings { ... }::parse` (validate!) and `build(ev: &OutEvent, settings, secret) -> Result<Built, String>`
   (`Built::Skip(reason)` when the event does not apply: sandbox, missing device id, disabled kind).
   New client ids → `attribution_key` (+ `ALL`). Unit tests next to the others.
2. `server/spacetimedb/src/config.rs`: secret name in `secret_name` + `is_known`.
3. `server/spacetimedb/src/integrations.rs`: `validate_settings`, `secret_for`, a `build_call` arm.
   An OAuth token (like Cloud Storage) is fetched in `send_integration_event` via `stores::google_token`.
4. `server/spacetimedb/src/doctor.rs`: title in `INTEGRATION_TITLES`, a verdict arm in `check_integrations`.
   Integrations keyed by mobile device ids skip builds without a mobile OS
   (`OutEvent::skip_unless_mobile`, `registry::os_of`).
5. Admin UI: `components/integrations.tsx` (`Kind`, `KINDS` entry, form fields in `IntegrationForm`),
   `pages/app.tsx` (`INTEGRATION_SECRET`, `ACCOUNT_SECRETS` if account-wide), `lib/types.ts` (`SECRET_NAMES`).
6. Unity: `AttributionKeys` constants for new client ids; mention them in the package README.
7. Docs: integrations table in `server/README.md`, both changelogs. A module test in `tests.rs`.

### Add an admin reducer / procedure

Reducer for writes (transactional): first `let who = crate::access::require(ctx, Cap::…)?;` with the
capability the action needs (`purser_core::roles`: `Read`, `Support`, `Configure`, `Secrets`, `Admins`,
`Owner`), then `crate::access::audit(ctx, &who, "<reducer name>", target, detail)` (never pass secret
values; `roles::audit_summary` hides keys named like secrets). Reads check `Cap::Read`. A new table
the admin page reads gets an arm in `access::read` (admins who are not the owner read through
`admin_select`; `/sql` is the owner's). `#[procedure]` when it needs
outbound HTTP or returns data: take `&mut ProcedureContext`, check the admin inside `ctx.tx`, return a
JSON string. Call it from the UI with `call()` (reducers) or `proc()` (procedures).

### Add an admin page or app tab

Page component in `pages/`, register it in `routes.tsx` (`PAGES` or `APP_TABS`); app tabs get
`TabProps` (`data.group`, `data.accounts`, `data.creds`, `data.integrations`, `reload`, `go`).

## Before you finish

- `cargo test -p purser-core` and `cargo test -p purser` pass; the wasm builds.
- If anything under `server/admin-ui` changed: `npm run build` and commit `admin/index.html`.
- New Unity files have `.meta` files; Unity code compiles in your head at least — CI does not run
  Unity tests, so keep changes small and covered by EditMode tests in `Tests/Editor`.
- Schema and client API changes follow the rules above (a `--delete-data` release says so); docs and
  changelogs are updated.
