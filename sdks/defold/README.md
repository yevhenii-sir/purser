# Purser for Defold

A Lua client of a self-hosted [Purser](../../README.md) server: verify, restore, entitlements, paywalls, promo codes,
wallet. It speaks the same HTTP contract as the Unity package and uses `http.request`; the store purchase itself stays
with [extension-iap](https://github.com/defold/extension-iap).

## Install

As a library dependency: Defold reads `game.project` at the root of a library zip, and this folder's `game.project`
shares `purser` (`[library] include_dirs = purser`). Make a zip of this folder and host it (a release asset, your own
server):

```bash
git archive --format=zip -o purser-defold.zip HEAD:sdks/defold
```

then add its URL to your `game.project` and fetch libraries:

```ini
[project]
dependencies#0 = https://example.com/purser-defold.zip

[iap]
auto_finish_transactions = 0
```

Or copy the `purser` folder into your project. `auto_finish_transactions = 0` lets the game finish purchases only after
the server answered.

```lua
local purser = require("purser.purser")

local billing = purser.new({
	base_url = "https://stdb.example.com/v1/database/<your-billing-db>/route",
	app_id = "com.example.mygame",          -- the build's app_id (its store's build)
	user_id = player_id,
	-- Builds with client_auth = signed: a player token from your backend (HS256 JWT, docs/game-integration.md).
	player_token = function(user_id, done)
		backend.purser_token(user_id, done)   -- done(token) when it arrives
	end,
})
```

Every call takes `callback(result)` with `result = { http_status, value, error, code, retry_after }` (`http_status` 0:
the server was not reached). Requests are retried after a transport error, a 5xx or a 429 (waiting its `Retry-After`
when Defold passes the header, at most 60 s; otherwise backing off), never after another 4xx (`retries`, `retry_delay`,
`timeout`). A `401 ERR_CLIENT_AUTH` asks `player_token` once more and sends the request again once. `client.sdk_version`
is `defold/<version>`; device, OS and `project.version` come from `sys`.

Signed answers (`X-Purser-Signature`, Ed25519) are not checked: Defold has no Ed25519. Use player tokens instead.

## Purchases with extension-iap

The store purchase, then `verify`, then the store's finish step only after the server's answer. `process_purchase`
applies the Unity package's rules:

| Server answer | Store purchase |
|---|---|
| `success`, `pending` | `handlers.grant(answer)` first when `delivery` is `client` (return `false` if it failed: the purchase stays unfinished), then `handlers.finish(true)` |
| `invalid`, `forbidden`, `expired`, `revoked` | `handlers.finish(false)`: Apple / Amazon finish it; leave a **Google** purchase unacknowledged (Google refunds it in 3 days) |
| `pending_or_canceled`, no network, 429, 5xx, `error` | nothing: the store reports it again and it is verified again |

```lua
local function iap_listener(self, transaction, error)
	if error or transaction.state ~= iap.TRANS_STATE_PURCHASED then
		return
	end
	local store = purser.store_of(iap.get_provider_id())    -- "google", "apple", "amazon"
	local purchase = purser.purchase_of(transaction, store)  -- product_id, purchase_token / order_id, store_user_id
	purchase.price_micros, purchase.currency = price_of(transaction.ident)  -- from iap.list
	billing:process_purchase(store, purchase, {
		grant = function(answer)                -- only for delivery = client builds
			inventory.add(answer.product_id, answer.quantity or 1)
			return save_game()
		end,
		finish = function(granted)
			if granted then
				iap.finish(transaction)          -- Google: consumes; use iap.acknowledge for non-consumables
			elseif store ~= "google" then
				iap.finish(transaction)          -- refused: Google purchases stay unacknowledged and are refunded
			end
		end,
	})
end

function init(self)
	iap.set_listener(iap_listener)
end
```

Google binds purchases with `obfuscatedAccountId` = the player id (`purser.google_account_id`; ids longer than 64
characters need SHA-256, which Defold has only through extension-crypt).

## Restore and entitlements

```lua
billing:restore({ { product_id = "no_ads", purchase_token = token } }, function(result) ... end)
billing:entitlements(function(result)
	if purser.has_access(result.value, "premium") then unlock_premium() end
end)
```

## Paywalls, attributes, promo codes, wallet

```lua
billing:paywall("main_paywall", function(r) show(r.value) end)          -- products, remote_config, variant_id, test_id
billing:paywall_event(shown_paywall, function() end)                    -- the paywall was shown
billing:set_attributes({ level = 7, old_key = "" }, function() end)     -- "" removes a key
billing:redeem("SUMMER-2026", function(r) end)                          -- the shape of /verify; ERR_CODE_* on 404 / 409
billing:wallet(function(r) print(r.value.balances.gems) end)
billing:spend("gems", 30, "sword-42", function(r) end)                  -- builds with player tokens; retry with the same reference
```

Pass the `/paywall` answer as `paywall` in the purchase for A/B test results.

## Tests

Plain Lua (5.1-5.4 or LuaJIT) with a stubbed `http.request`, from this folder:

```bash
lua tests/test_purser.lua                          # exit code 0 when every check passes
UPDATE_SDK_REQUESTS=1 lua tests/test_purser.lua    # rewrites server/spacetimedb/sdk-requests/defold.json
```

`server/spacetimedb/sdk-requests/defold.json` holds what the module sends; the module's tests parse every body.
