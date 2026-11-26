# Purser for Godot 4

A GDScript client of a self-hosted [Purser](../../README.md) server (Godot 4.2+): verify, restore, entitlements,
paywalls, promo codes, wallet. It speaks the same HTTP contract as the Unity package and uses `HTTPRequest`; the store
purchase itself stays with the engine's IAP plugin.

## Install

Copy `addons/purser` into your project's `addons/` folder. It is a script library (`PurserClient`), not an editor
plugin: nothing to enable.

```gdscript
var purser := PurserClient.new()

func _ready() -> void:
	add_child(purser)
	purser.configure({
		"base_url": "https://stdb.example.com/v1/database/<your-billing-db>/route",
		"app_id": "com.example.mygame",            # the build's app_id (its store's build)
		"user_id": player_id,
		# Builds with client_auth = signed: a player token from your backend (HS256 JWT, docs/game-integration.md).
		"player_token_provider": func(uid: String) -> String: return await backend.purser_token(uid),
	})
```

Every call is awaited and returns `{"http_status", "value", "error", "code", "retry_after"}` (`http_status` 0: the
server was not reached). Requests are retried after a transport error, a 5xx or a 429 (waiting its `Retry-After`, at most
60 s), never after another 4xx (`retries`, `retry_delay`, `timeout_seconds`). A `401 ERR_CLIENT_AUTH` asks the token
provider once more and sends the request again once. `client.sdk_version` is `godot/<version>`; `app_version` comes from
`application/config/version`.

Signed answers (`X-Purser-Signature`, Ed25519) are not checked: Godot has no Ed25519. Use player tokens instead.

## Purchases

The store purchase, then `verify`, then the store's finish step only after the server's answer. `process_purchase`
applies the Unity package's rules:

| Server answer | Store purchase |
|---|---|
| `success`, `pending` | `grant` first when `delivery` is `client` (return `false` if it failed: the purchase stays unfinished), then `finish.call(true)` |
| `invalid`, `forbidden`, `expired`, `revoked` | `finish.call(false)`: Apple `finish_transaction`; leave a **Google** purchase unacknowledged (Google refunds it in 3 days) |
| `pending_or_canceled`, no network, 429, 5xx, `error` | nothing: the store reports it again and it is verified again |

### Google Play (official GodotGooglePlayBilling plugin)

Names from the plugin's API reference (`BillingClient`, Godot 4.2+); check them against the version you install.

```gdscript
var billing := BillingClient.new()

func _ready() -> void:
	billing.on_purchase_updated.connect(_on_purchases)
	billing.query_purchases_response.connect(_on_purchases)   # unfinished purchases after a restart
	billing.connected.connect(func(): billing.query_purchases(BillingClient.ProductType.INAPP))
	billing.start_connection()

func buy(product_id: String) -> void:
	# Binds the purchase to the player: the server refuses consumables bound to someone else.
	billing.set_obfuscated_account_id(PurserClient.google_account_id(purser.user_id))
	billing.purchase(product_id)

func _on_purchases(response: Dictionary) -> void:
	for p: Dictionary in response.get("purchases", []):
		if p.purchase_state != BillingClient.PurchaseState.PURCHASED:
			continue
		var done := await purser.process_purchase("google", {
			"product_id": p.product_ids[0],
			"purchase_token": p.purchase_token,
			"order_id": p.order_id,
			"price_micros": price_micros_of(p.product_ids[0]),   # from query_product_details
			"currency": currency_of(p.product_ids[0]),
		}, _grant, func(granted: bool) -> void:
			if not granted:
				return   # refused: left unacknowledged, Google refunds it
			if is_consumable(p.product_ids[0]):
				billing.consume_purchase(p.purchase_token)
			else:
				billing.acknowledge_purchase(p.purchase_token)
		)

func _grant(answer: Dictionary) -> bool:
	inventory.add(answer.product_id, int(answer.get("quantity", 1)))
	return save_game()
```

### App Store (godot-ios-plugins `InAppStore`)

```gdscript
var store := Engine.get_singleton("InAppStore")

func _ready() -> void:
	store.set_auto_finish_transaction(false)

func _process(_delta: float) -> void:
	while store.get_pending_event_count() > 0:
		var event: Dictionary = store.pop_pending_event()
		if event.type == "purchase" and event.result == "ok":
			purser.process_purchase("apple", {"product_id": event.product_id, "order_id": event.transaction_id}, _grant,
				func(_granted: bool) -> void: store.finish_transaction(event.product_id))
```

Apple finishes the transaction both after a success and after a refusal. StoreKit's `appAccountToken`
(`PurserClient.apple_account_token(user_id)`) binds consumables to the player when the plugin can pass it.

## Restore and entitlements

```gdscript
var restored := await purser.restore([{"product_id": "no_ads", "purchase_token": token}])
var info := await purser.entitlements()
if PurserClient.has_access(info.value, "premium"):
	unlock_premium()
```

## Paywalls, attributes, promo codes, wallet

```gdscript
var paywall := await purser.paywall("main_paywall")      # value: products, remote_config, variant_id, test_id
await purser.paywall_event(paywall.value)                 # the paywall was shown
await purser.set_attributes({"level": 7, "old_key": null})
var redeemed := await purser.redeem("SUMMER-2026")        # the shape of /verify; ERR_CODE_* on 404 / 409
var wallet := await purser.wallet()                       # value.balances = {"gems": 120}
var spent := await purser.spend("gems", 30, "sword-42")   # builds with player tokens; retry with the same reference
```

Pass the `/paywall` answer as `"paywall"` in the purchase for A/B test results.

## Tests

```bash
godot --headless --path sdks/godot -s tests/test_purser.gd   # exit code 0 when every check passes
```

A fake transport, no network, no GUT. `server/spacetimedb/sdk-requests/godot.json` holds what the addon sends; the test
compares it (`UPDATE_SDK_REQUESTS=1` rewrites it) and the module's tests parse every body.
