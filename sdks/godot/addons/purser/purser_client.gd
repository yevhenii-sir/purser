class_name PurserClient
extends Node
## Client of a self-hosted Purser in-app purchase server (verify, restore, entitlements, paywalls,
## promo codes, wallet). Add it to the tree, call [method configure], then [code]await[/code] its methods.
## Every call returns a result Dictionary:
## [code]{"http_status": int (0 = not reached), "value": the parsed body or null, "error": String ("" on
## success), "code": the error code ("ERR_..."), "retry_after": seconds of a 429 or -1}[/code].

const SDK_VERSION := "0.13.0"
const MAX_RESTORE_ITEMS := 20
## A [code]Retry-After[/code] longer than this (seconds) is cut to it.
const MAX_RETRY_AFTER := 60.0
const ERR_CLIENT_AUTH := "ERR_CLIENT_AUTH"

## What a [code]/verify[/code] answer means for the purchase ([method classify]).
const OUTCOME_SUCCESS := "success"
const OUTCOME_PENDING_DELIVERY := "pending_delivery"
const OUTCOME_PENDING_PAYMENT := "pending_payment"
const OUTCOME_REJECTED := "rejected"
const OUTCOME_RETRY := "retry"

## What to do with the store purchase ([method finish_action]).
const FINISH := "finish"
const FINISH_REFUSED := "finish_refused"
const KEEP := "keep"

## The billing server: [code]https://<host>/v1/database/<db>/route[/code].
var base_url := ""
## This build's [code]app_id[/code].
var app_id := ""
## The player; set it after sign-in.
var user_id := ""
## [code]device[/code], [code]os[/code], [code]app_version[/code], [code]sdk_version[/code] shown on the admin page.
var client_info := {}
var timeout_seconds := 30.0
## Retries of a request after a transport error, a 5xx or a 429 (never after another 4xx).
var retries := 3
## First retry delay in seconds, doubled each time (a 429 waits its [code]Retry-After[/code]).
var retry_delay := 1.0
## Builds with [code]client_auth = signed[/code]: [code]func(user_id: String) -> String[/code] (may await) giving a
## player token from the game's backend. It is kept until the server refuses it (401 ERR_CLIENT_AUTH); then it is
## asked again and the request is sent again once.
var player_token_provider: Callable
## Test hook: [code]func(url: String, headers: PackedStringArray, body: String) -> Dictionary[/code]
## ([code]{"status": int, "headers": {lowercase name: value}, "body": String, "error": String}[/code]).
var transport: Callable
## Test hook: [code]func(seconds: float)[/code] awaited between retries.
var sleeper: Callable

var _tokens := {}


## Settings: [code]base_url[/code], [code]app_id[/code], [code]user_id[/code], [code]app_version[/code],
## [code]timeout_seconds[/code], [code]retries[/code], [code]retry_delay[/code], [code]player_token_provider[/code].
func configure(options: Dictionary) -> Node:
	base_url = _s(options.get("base_url")).rstrip("/")
	app_id = _s(options.get("app_id"))
	user_id = _s(options.get("user_id"))
	timeout_seconds = float(options.get("timeout_seconds", timeout_seconds))
	retries = int(options.get("retries", retries))
	retry_delay = float(options.get("retry_delay", retry_delay))
	if options.has("player_token_provider"):
		player_token_provider = options["player_token_provider"]
	client_info = _default_client(_s(options.get("app_version")))
	var device: Variant = options.get("client")
	if device is Dictionary:
		for key: String in ["device", "os", "app_version"]:
			if device.has(key):
				client_info[key] = _s(device[key])
	return self


# --------------------------------------------------------------------------------------------
# Purchases
# --------------------------------------------------------------------------------------------

## [code]POST /verify/<store>[/code] ([code]google[/code], [code]apple[/code], [code]amazon[/code], ...). [param purchase]:
## [code]product_id[/code], [code]purchase_token[/code] (Google), [code]order_id[/code] (Apple transactionId, Amazon
## receiptId), [code]store_user_id[/code] (Amazon userId), [code]price_micros[/code], [code]currency[/code],
## [code]app_instance_id[/code], [code]attribution[/code], [code]paywall[/code] (a [method paywall] answer),
## [code]offer_id[/code], [code]client_finished[/code].
func verify(store: String, purchase: Dictionary) -> Dictionary:
	return await post("/verify/" + store.uri_encode(), verify_body(purchase))


## The [code]/verify[/code] body of a purchase.
func verify_body(p: Dictionary) -> Dictionary:
	var body := {"app_id": app_id, "user_id": user_id, "product_id": _s(p.get("product_id"))}
	for key: String in ["purchase_token", "order_id", "store_user_id", "app_instance_id"]:
		_put(body, key, p.get(key))
	body["price_micros"] = int(p.get("price_micros", 0))
	_put(body, "currency", p.get("currency"))
	var attribution: Variant = p.get("attribution")
	if attribution is Dictionary and not attribution.is_empty():
		body["attribution"] = attribution
	_put_paywall(body, p.get("paywall"))
	_put(body, "offer_id", p.get("offer_id"))
	body["client"] = client_info
	if p.get("client_finished", false):
		body["client_finished"] = true
	return body


## Verifies a store purchase and applies the Unity package's rules: on [code]success[/code] / [code]pending[/code]
## [param grant] (called only when the server answers [code]delivery: client[/code], with the answer; return
## [code]false[/code] when granting failed) runs first, then [code]finish.call(true)[/code]; a refusal calls
## [code]finish.call(false)[/code]; anything else leaves the purchase unfinished, so the store delivers it again.
## Returns [code]{"outcome", "action", "result"}[/code].
func process_purchase(store: String, purchase: Dictionary, grant: Callable, finish: Callable) -> Dictionary:
	var result := await verify(store, purchase)
	return await apply_outcome(result, grant, finish)


## The rules of [method process_purchase] for an answer already received.
func apply_outcome(result: Dictionary, grant: Callable, finish: Callable) -> Dictionary:
	var outcome := classify(result)
	var action := finish_action(outcome)
	var value: Variant = result.get("value")
	if action == FINISH:
		if value is Dictionary and value.get("delivery") == "client" and grant.is_valid():
			var granted: Variant = await grant.call(value)
			if granted is bool and not granted:
				# Not finished: the store delivers it again, the server answers success from its ledger.
				return {"outcome": outcome, "action": KEEP, "result": result}
		await finish.call(true)
	elif action == FINISH_REFUSED:
		await finish.call(false)
	return {"outcome": outcome, "action": action, "result": result}


## What a [code]/verify[/code] answer means: [code]OUTCOME_*[/code].
static func classify(result: Dictionary) -> String:
	var http := int(result.get("http_status", 0))
	if http == 0 or http == 429:
		return OUTCOME_RETRY
	var value: Variant = result.get("value")
	var status := ""
	if value is Dictionary and value.get("status") is String:
		status = value["status"]
	match status:
		"success":
			return OUTCOME_SUCCESS
		"pending":
			return OUTCOME_PENDING_DELIVERY
		"pending_or_canceled":
			return OUTCOME_PENDING_PAYMENT
		"invalid", "forbidden", "expired", "revoked":
			return OUTCOME_REJECTED
	# `error` (server setup, store outage, refused player token), 5xx, an unexpected body.
	return OUTCOME_RETRY


## [code]FINISH[/code]: finish as granted (Google consume / acknowledge, Apple finish_transaction).
## [code]FINISH_REFUSED[/code]: finish as refused on Apple; leave a Google purchase unacknowledged (Google refunds it).
## [code]KEEP[/code]: do not finish it.
static func finish_action(outcome: String) -> String:
	match outcome:
		OUTCOME_SUCCESS, OUTCOME_PENDING_DELIVERY:
			return FINISH
		OUTCOME_REJECTED:
			return FINISH_REFUSED
	return KEEP


## [code]POST /restore[/code]: [param purchases] are [code]{"product_id", "purchase_token"?, "order_id"?,
## "store_user_id"?}[/code] (at most 20); [param options]: [code]store_user_id[/code], [code]attribution[/code],
## [code]client_finished[/code]. Answer: [code]{results, entitlements}[/code].
func restore(purchases: Array, options: Dictionary = {}) -> Dictionary:
	if purchases.size() > MAX_RESTORE_ITEMS:
		return _failure("at most %d purchases per restore call" % MAX_RESTORE_ITEMS)
	var body := {"app_id": app_id, "user_id": user_id}
	_put(body, "store_user_id", options.get("store_user_id"))
	_put(body, "app_instance_id", options.get("app_instance_id"))
	var attribution: Variant = options.get("attribution")
	if attribution is Dictionary and not attribution.is_empty():
		body["attribution"] = attribution
	var items := []
	for p: Dictionary in purchases:
		var item := {"product_id": _s(p.get("product_id"))}
		for key: String in ["purchase_token", "order_id", "store_user_id"]:
			_put(item, key, p.get(key))
		items.append(item)
	body["purchases"] = items
	body["client"] = client_info
	if options.get("client_finished", false):
		body["client_finished"] = true
	return await post("/restore", body)


## [code]POST /entitlements[/code]: [code]{access, subscriptions, non_consumables, paused?, server_time_ms}[/code].
func entitlements() -> Dictionary:
	return await post("/entitlements", {"app_id": app_id, "user_id": user_id, "client": client_info})


## Whether an [method entitlements] answer gives [param access] (e.g. [code]premium[/code]) now.
static func has_access(customer_info: Variant, access: String) -> bool:
	if not customer_info is Dictionary or not customer_info.get("access") is Dictionary:
		return false
	var state: Variant = customer_info["access"].get(access)
	if not state is Dictionary:
		return false
	var expires := float(state.get("expires_at_ms", 0))
	return expires == 0 or expires > float(customer_info.get("server_time_ms", 0))


# --------------------------------------------------------------------------------------------
# Paywalls, attributes, promo codes, wallet
# --------------------------------------------------------------------------------------------

## [code]POST /paywall[/code]: what to show at [param placement].
func paywall(placement: String) -> Dictionary:
	var body := {"app_id": app_id, "placement": placement}
	_put(body, "user_id", user_id)
	body["client"] = client_info
	return await post("/paywall", body)


## [code]POST /paywall/event[/code]: the [method paywall] answer [param shown] was shown.
func paywall_event(shown: Dictionary, event := "view") -> Dictionary:
	var body := {"app_id": app_id, "placement": _s(shown.get("placement")), "variant_id": _s(shown.get("variant_id"))}
	_put(body, "test_id", shown.get("test_id"))
	_put(body, "user_id", user_id)
	body["event"] = event
	return await post("/paywall/event", body)


## [code]POST /attributes[/code]: custom attributes; [code]null[/code] or [code]""[/code] removes a key.
func set_attributes(attributes: Dictionary) -> Dictionary:
	return await post("/attributes", {"app_id": app_id, "user_id": user_id, "attributes": attributes, "client": client_info})


## [code]POST /redeem[/code]: a promo code; the answer has the shape of [code]/verify[/code].
func redeem(code: String) -> Dictionary:
	return await post("/redeem", {"app_id": app_id, "user_id": user_id, "code": code, "client": client_info})


## [code]POST /wallet[/code]: [code]{"status": "ok", "balances": {"gems": 120}}[/code].
func wallet() -> Dictionary:
	return await post("/wallet", {"app_id": app_id, "user_id": user_id, "client": client_info})


## [code]POST /wallet/spend[/code] (builds with player tokens). Retry a failed spend with the same [param reference].
func spend(currency: String, amount: int, reference: String, note := "") -> Dictionary:
	var body := {"app_id": app_id, "user_id": user_id, "currency": currency, "amount": amount, "reference": reference}
	_put(body, "note", note)
	body["client"] = client_info
	return await post("/wallet/spend", body)


# --------------------------------------------------------------------------------------------
# Account binding (the server checks consumables against it)
# --------------------------------------------------------------------------------------------

## Google [code]obfuscatedAccountId[/code]: the player id, or the SHA-256 hex of an id longer than 64 characters.
static func google_account_id(uid: String) -> String:
	if uid.length() <= 64:
		return uid
	return uid.sha256_text().substr(0, 64)


## Apple [code]appAccountToken[/code]: a UUID (version 8) from the first 16 bytes of SHA-256 of the player id.
static func apple_account_token(uid: String) -> String:
	var ctx := HashingContext.new()
	ctx.start(HashingContext.HASH_SHA256)
	ctx.update(uid.to_utf8_buffer())
	var b := ctx.finish().slice(0, 16)
	b[6] = (b[6] & 0x0f) | 0x80
	b[8] = (b[8] & 0x3f) | 0x80
	var h := b.hex_encode()
	return "%s-%s-%s-%s-%s" % [h.substr(0, 8), h.substr(8, 4), h.substr(12, 4), h.substr(16, 4), h.substr(20, 12)]


# --------------------------------------------------------------------------------------------
# Transport
# --------------------------------------------------------------------------------------------

## POSTs [param body] to [param path] with the player token, retrying transport errors, 5xx and 429.
## [param for_user]: the player the token is for (default [member user_id]).
func post(path: String, body: Dictionary, for_user: Variant = null) -> Dictionary:
	var user := user_id if for_user == null else _s(for_user)
	var json := JSON.stringify(body)
	var refreshed := false
	var attempt := 0
	while true:
		var token := ""
		if player_token_provider.is_valid() and user != "":
			if _tokens.has(user):
				token = _tokens[user]
			else:
				var fresh: Variant = await player_token_provider.call(user)
				token = _s(fresh)
				if token != "":
					_tokens[user] = token
		var raw := await _send(path, json, token)
		var result := _parse(raw)
		var status := int(result["http_status"])
		if status == 401 and result["code"] == ERR_CLIENT_AUTH and player_token_provider.is_valid() and user != "" and not refreshed:
			# The token expired or was revoked: ask the game's backend once more.
			refreshed = true
			if _tokens.get(user, "") == token:
				_tokens.erase(user)
			continue
		var retryable := status == 0 or status == 429 or status >= 500
		if not retryable or attempt >= retries:
			return result
		var wait := retry_delay * pow(2.0, attempt)
		if status == 429 and float(result["retry_after"]) >= 0.0:
			wait = float(result["retry_after"])
		attempt += 1
		await _sleep(wait)
	return _failure("unreachable")


func _send(path: String, json: String, token: String) -> Dictionary:
	var headers := PackedStringArray(["Content-Type: application/json"])
	if token != "":
		headers.append("Authorization: Bearer " + token)
	var url := base_url + path
	if transport.is_valid():
		var faked: Variant = await transport.call(url, headers, json)
		return faked
	var http := HTTPRequest.new()
	http.timeout = timeout_seconds
	add_child(http)
	var err := http.request(url, headers, HTTPClient.METHOD_POST, json)
	if err != OK:
		http.queue_free()
		return {"status": 0, "error": "request not sent: %s" % error_string(err)}
	var done: Array = await http.request_completed
	http.queue_free()
	if int(done[0]) != HTTPRequest.RESULT_SUCCESS:
		return {"status": 0, "error": "network error (HTTPRequest result %d)" % int(done[0])}
	var response_headers := {}
	for line: String in done[2]:
		var i := line.find(":")
		if i > 0:
			response_headers[line.substr(0, i).strip_edges().to_lower()] = line.substr(i + 1).strip_edges()
	var bytes: PackedByteArray = done[3]
	return {"status": int(done[1]), "headers": response_headers, "body": bytes.get_string_from_utf8()}


func _parse(raw: Dictionary) -> Dictionary:
	var status := int(raw.get("status", 0))
	if status == 0:
		return _failure(_s(raw.get("error", "network error")))
	var text := _s(raw.get("body"))
	var value: Variant = null
	var error := ""
	if text != "":
		var parser := JSON.new()
		if parser.parse(text) == OK:
			value = parser.data
		else:
			error = "invalid JSON from server: " + parser.get_error_message()
	var ok := status >= 200 and status < 300
	var code := ""
	if not ok and value is Dictionary and value.get("code") is String:
		code = value["code"]
	var retry_after := -1.0
	if status == 429:
		var headers: Variant = raw.get("headers", {})
		var header := _s(headers.get("retry-after")) if headers is Dictionary else ""
		if header.is_valid_float():
			retry_after = clampf(header.to_float(), 0.0, MAX_RETRY_AFTER)
	if error == "" and not ok:
		error = "HTTP %d: %s" % [status, text.left(300)]
	return {"http_status": status, "value": value, "error": error, "code": code, "retry_after": retry_after}


func _sleep(seconds: float) -> void:
	if sleeper.is_valid():
		await sleeper.call(seconds)
	elif is_inside_tree():
		await get_tree().create_timer(seconds).timeout


func _failure(message: String) -> Dictionary:
	return {"http_status": 0, "value": null, "error": message, "code": "", "retry_after": -1.0}


func _default_client(app_version: String) -> Dictionary:
	if app_version == "":
		app_version = _s(ProjectSettings.get_setting("application/config/version", ""))
	var os_name := OS.get_name()
	if OS.has_method("get_version"):
		os_name += " " + _s(OS.call("get_version"))
	return {"device": OS.get_model_name(), "os": os_name.strip_edges(), "app_version": app_version, "sdk_version": "godot/" + SDK_VERSION}


static func _s(v: Variant) -> String:
	return "" if v == null else str(v)


## Sets [param key] unless [param v] is null or empty (the server reads a missing field as its default).
static func _put(body: Dictionary, key: String, v: Variant) -> void:
	var text := _s(v)
	if text != "":
		body[key] = text


static func _put_paywall(body: Dictionary, paywall: Variant) -> void:
	if paywall is Dictionary:
		for key: String in ["placement", "variant_id", "test_id", "paywall_id"]:
			_put(body, key, paywall.get(key))
