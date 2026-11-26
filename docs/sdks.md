# Client SDKs for other engines

Besides the [Unity package](../unity/com.yevhenii.purser/README.md), `sdks/` has clients for other engines. They speak
the same HTTP contract (server/README.md, "HTTP API"), send the same `client` info and follow the Unity package's
rules for finishing purchases. The store purchase itself stays with the engine's IAP plugin.

| SDK | For | Folder | Store purchases through |
|---|---|---|---|
| `@purser/client` (TypeScript, `fetch`, no dependencies) | web games, Cocos Creator, PlayCanvas, Telegram Mini Apps | [sdks/typescript](../sdks/typescript/README.md) | Stripe Checkout, Telegram Stars (helpers included), any native IAP plugin |
| Godot 4 addon (GDScript, `HTTPRequest`) | Godot 4.2+ | [sdks/godot](../sdks/godot/README.md) | GodotGooglePlayBilling, godot-ios-plugins `InAppStore` |
| Defold module (Lua, `http.request`) | Defold | [sdks/defold](../sdks/defold/README.md) | extension-iap |

## What each covers

| | TypeScript | Godot | Defold |
|---|---|---|---|
| `/verify/<store>` with `price_micros`, `currency`, `attribution`, paywall ids, `offer_id`, `client_finished` | yes | yes | yes |
| `/restore`, `/entitlements` (access levels, subscriptions) | yes | yes | yes |
| `/paywall`, `/paywall/event` | yes | yes | yes |
| `/attributes`, `/redeem` | yes | yes | yes |
| `/wallet`, `/wallet/spend` | yes | yes | yes |
| Stripe and Telegram checkout | yes | `post(path, body)` | `post(path, body, callback)` |
| Player tokens (`client_auth = signed`), refreshed once after `401 ERR_CLIENT_AUTH` | yes | yes | yes |
| Retries: transport errors, 5xx, 429 with `Retry-After` (at most 60 s); never other 4xx | yes | yes | yes (`Retry-After` when Defold passes the header) |
| Signed answers (`X-Purser-Signature`, Ed25519) | yes (Web Crypto) | no (no Ed25519 in Godot) | no |
| Account binding ids (Google `obfuscatedAccountId`, Apple `appAccountToken`) | both | both | Google, ids up to 64 characters |

`client.sdk_version` is `typescript/<version>`, `godot/<version>` or `defold/<version>`, so the admin page's player
profile shows which SDK a player runs.

## Finishing store purchases

The flow is the same everywhere: the store purchase, `verify`, then the store's finish step only after the server's
answer. Each SDK's `processPurchase` / `process_purchase` applies it:

| Server answer | Outcome | Store purchase |
|---|---|---|
| `success` (200), `pending` (202) | success | grant the reward first when the answer has `delivery: client` (a failed grant leaves the purchase unfinished), then finish it as granted |
| `invalid`, `forbidden`, `expired`, `revoked` | rejected | finish it as refused: Apple `finishTransaction`, Amazon `notifyFulfillment(UNAVAILABLE)`; a Google purchase is left unacknowledged, so Google refunds it in three days instead of the player losing the money to a setup mistake |
| `pending_or_canceled` | pending payment | keep it: the store reports it again when it is paid |
| no network, 429, 5xx, `error` (after the SDK's retries) | retry | keep it: the store delivers it again (next launch) and it is verified again; the server answers from its ledger |

Turn off the IAP plugin's automatic finishing (`auto_finish_transactions = 0` in Defold, `set_auto_finish_transaction(false)`
for StoreKit in Godot). With `delivery: client`, save the granted `transaction_id`: a purchase delivered again answers
`success` with the same id.

## The contract with the server

Every SDK's requests are kept in `server/spacetimedb/sdk-requests/` (`current.json` for Unity, `typescript.json`,
`godot.json`, `defold.json`), written or compared by the SDK's own tests. The module's tests
`every_request_of_every_sdk_is_accepted` and `the_purchase_flow_of_every_sdk_works` parse every body with the server's
request types, refuse fields the server does not read, and replay a verify, the entitlements and the attributes.

| SDK | Tests |
|---|---|
| TypeScript | `cd sdks/typescript && npm install && npm test` (Node 22+, a fake `fetch`) |
| Godot | `godot --headless --path sdks/godot -s tests/test_purser.gd` (a fake transport, no GUT) |
| Defold | `cd sdks/defold && lua tests/test_purser.lua` (stock Lua 5.1-5.4 or LuaJIT, a stubbed `http.request`) |
