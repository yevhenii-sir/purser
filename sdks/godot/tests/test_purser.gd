extends SceneTree
## Tests of the Purser addon without the network or GUT:
##   godot --headless --path sdks/godot -s tests/test_purser.gd
## Exit code 0 when every check passes. UPDATE_SDK_REQUESTS=1 rewrites
## server/spacetimedb/sdk-requests/godot.json from what the addon sends.

const Client := preload("res://addons/purser/purser_client.gd")
const BASE := "https://stdb.example.com/v1/database/billing/route"

var failures := 0
var calls: Array = []
var answers: Array = []
var sleeps: Array = []


func _initialize() -> void:
	_run()


func _run() -> void:
	await _test_verify_body()
	await _test_store()
	await _test_retries()
	await _test_rate_limit()
	await _test_no_retry_on_4xx()
	await _test_player_tokens()
	_test_classify()
	await _test_process_purchase()
	_test_account_ids()
	await _test_contract()
	print("Purser addon tests: %s" % ("all passed" if failures == 0 else "%d failed" % failures))
	quit(1 if failures > 0 else 0)


func check(ok: bool, what: String) -> void:
	if not ok:
		failures += 1
		push_error("FAILED: " + what)


func _client(extra := {}):
	calls = []
	sleeps = []
	var c = Client.new()
	var options := {
		"base_url": BASE + "/",
		"app_id": "com.studio.game",
		"store": "google",
		"user_id": "player-1",
		"client": {"device": "Pixel 8", "os": "Android 15", "app_version": "1.2.1"},
	}
	options.merge(extra, true)
	c.configure(options)
	c.transport = _fake
	c.sleeper = func(seconds: float) -> void: sleeps.append(seconds)
	return c


func _fake(url: String, headers: PackedStringArray, body: String) -> Dictionary:
	calls.append({"path": url.substr(BASE.length()), "headers": headers, "body": JSON.parse_string(body)})
	var a: Dictionary = answers[0] if answers.size() == 1 else answers.pop_front()
	return a


func _answer(status: int, body: Variant, headers := {}) -> Dictionary:
	return {"status": status, "body": JSON.stringify(body) if body != null else "", "headers": headers}


func _has_auth(headers: PackedStringArray) -> String:
	for h in headers:
		if h.begins_with("Authorization: "):
			return h.substr("Authorization: ".length())
	return ""


func _test_verify_body() -> void:
	answers = [_answer(200, {"status": "success", "transaction_id": "GPA.1", "delivery": "server"})]
	var c = _client()
	var r: Dictionary = await c.verify("google", {"product_id": "gems", "purchase_token": "tok-1", "order_id": "", "price_micros": 990000, "currency": "USD"})
	check(r["http_status"] == 200 and r["value"]["status"] == "success", "verify answer")
	check(calls[0]["path"] == "/verify/google", "verify path")
	check(_has_auth(calls[0]["headers"]) == "", "no token without a provider")
	var body: Dictionary = calls[0]["body"]
	check(not body.has("order_id"), "empty fields are left out")
	check(body["price_micros"] == 990000 and body["client"]["sdk_version"] == "godot/0.14.0", "price and sdk version")
	c.free()


func _test_store() -> void:
	answers = [_answer(200, {"status": "ok"})]
	var c = _client({"store": "apple"})
	await c.entitlements()
	await c.restore([{"product_id": "no_ads", "order_id": "2000000001"}])
	await c.verify("apple", {"product_id": "gems", "order_id": "2000000123"})
	check(calls[0]["body"]["app_id"] == "com.studio.game" and calls[0]["body"]["store"] == "apple", "entitlements name the build's store")
	check(calls[1]["body"]["store"] == "apple", "restore names the build's store")
	check(not calls[2]["body"].has("store"), "verify names the store in its path")
	c.free()


func _test_retries() -> void:
	answers = [{"status": 0, "error": "offline"}, _answer(502, {"status": "error"}), _answer(200, {"access": {}})]
	var c = _client()
	var r: Dictionary = await c.entitlements()
	check(r["http_status"] == 200 and calls.size() == 3, "transport error and 5xx retried")
	check(sleeps == [1.0, 2.0], "backoff %s" % [sleeps])
	c.free()


func _test_rate_limit() -> void:
	var limited := {"status": "error", "code": "ERR_RATE_LIMITED"}
	answers = [_answer(429, limited, {"retry-after": "7"}), _answer(429, limited, {"retry-after": "600"}), _answer(200, {"status": "ok"})]
	var c = _client()
	var r: Dictionary = await c.wallet()
	check(r["http_status"] == 200 and sleeps == [7.0, 60.0], "429 waits Retry-After, at most 60 s: %s" % [sleeps])
	c.free()


func _test_no_retry_on_4xx() -> void:
	for status in [400, 403, 404, 409]:
		answers = [_answer(status, {"status": "invalid", "code": "ERR_CODE_INVALID"})]
		var c = _client()
		var r: Dictionary = await c.redeem("SUMMER")
		check(calls.size() == 1 and sleeps.is_empty() and r["code"] == "ERR_CODE_INVALID", "HTTP %d is not retried" % status)
		c.free()


func _test_player_tokens() -> void:
	var issued := [0]
	var provider := func(user: String) -> String:
		issued[0] += 1
		return "token-%s-%d" % [user, issued[0]]
	var ok := _answer(200, {"access": {}})
	var refused := _answer(401, {"status": "error", "code": "ERR_CLIENT_AUTH"})
	answers = [ok, refused, ok, refused, refused]
	var c = _client({"player_token_provider": provider})
	await c.entitlements()
	check(_has_auth(calls[0]["headers"]) == "Bearer token-player-1-1", "token sent")
	var r: Dictionary = await c.entitlements()
	check(r["http_status"] == 200, "refreshed after 401")
	check(_has_auth(calls[2]["headers"]) == "Bearer token-player-1-2", "a new token")
	var again: Dictionary = await c.entitlements()
	check(again["http_status"] == 401 and calls.size() == 5, "refreshed once only")
	c.free()


func _test_classify() -> void:
	var cases := [
		[200, "success", Client.OUTCOME_SUCCESS, Client.FINISH],
		[202, "pending", Client.OUTCOME_PENDING_DELIVERY, Client.FINISH],
		[200, "pending_or_canceled", Client.OUTCOME_PENDING_PAYMENT, Client.KEEP],
		[400, "invalid", Client.OUTCOME_REJECTED, Client.FINISH_REFUSED],
		[403, "forbidden", Client.OUTCOME_REJECTED, Client.FINISH_REFUSED],
		[200, "expired", Client.OUTCOME_REJECTED, Client.FINISH_REFUSED],
		[200, "revoked", Client.OUTCOME_REJECTED, Client.FINISH_REFUSED],
		[400, "error", Client.OUTCOME_RETRY, Client.KEEP],
		[502, "error", Client.OUTCOME_RETRY, Client.KEEP],
		[429, "error", Client.OUTCOME_RETRY, Client.KEEP],
		[0, "", Client.OUTCOME_RETRY, Client.KEEP],
	]
	for c in cases:
		var result := {"http_status": c[0], "value": {"status": c[1]} if c[1] != "" else null}
		var outcome: String = Client.classify(result)
		check(outcome == c[2] and Client.finish_action(outcome) == c[3], "classify %s" % [c])


func _test_process_purchase() -> void:
	var events := []
	var grant := func(answer: Dictionary) -> bool:
		events.append("grant " + str(answer["transaction_id"]))
		return true
	var finish := func(granted: bool) -> void: events.append("finish %s" % granted)
	answers = [_answer(200, {"status": "success", "transaction_id": "GPA.1", "delivery": "client"})]
	var c = _client()
	var done: Dictionary = await c.process_purchase("google", {"product_id": "gems", "purchase_token": "tok-1"}, grant, finish)
	check(done["outcome"] == Client.OUTCOME_SUCCESS and events == ["grant GPA.1", "finish true"], "grant, then finish: %s" % [events])
	events.clear()
	var failing := func(_a: Dictionary) -> bool: return false
	done = await c.process_purchase("google", {"product_id": "gems", "purchase_token": "tok-1"}, failing, finish)
	check(done["action"] == Client.KEEP and events.is_empty(), "a failed grant keeps the purchase")
	answers = [_answer(403, {"status": "forbidden"})]
	await c.process_purchase("apple", {"product_id": "gems", "order_id": "1"}, grant, finish)
	check(events == ["finish false"], "refused: finished as refused")
	c.free()


func _test_account_ids() -> void:
	var identity := "c200fdb63a0e9a9b605e424fd437da9249eb198086994a486efa7f115244b8ba"
	check(Client.google_account_id(identity) == identity, "google id of a 64-char id")
	check(Client.google_account_id("x".repeat(65)) == "9537c5fdf120482f7d58d25e9ed583f52c02b4e304ea814db1633ad565aed7e9", "google id hash")
	check(Client.apple_account_token(identity) == "b355ff39-27d8-8191-b44b-07b0fa0ed98e", "apple token")
	check(Client.apple_account_token("player-42") == "c3214538-d133-89d9-b4db-63a435cddcae", "apple token 2")


## What the addon sends, compared with server/spacetimedb/sdk-requests/godot.json (parsed by the module's tests).
func _test_contract() -> void:
	answers = [_answer(200, {"status": "ok"})]
	var c = _client()
	await c.verify("google", {
		"product_id": "gems", "purchase_token": "tok-1", "order_id": "GPA.1111-2222-3333-44444",
		"price_micros": 990000, "currency": "USD",
		"paywall": {"placement": "store", "variant_id": "var-a", "test_id": "test-1", "paywall_id": "pw-1"},
		"offer_id": "monthly-intro",
	})
	await c.verify("apple", {"product_id": "gems", "order_id": "2000000123"})
	await c.restore([{"product_id": "no_ads", "purchase_token": "tok-old", "order_id": "GPA.1"}])
	await c.entitlements()
	await c.paywall("store")
	await c.paywall_event({"placement": "store", "variant_id": "var-a", "test_id": "test-1"})
	await c.set_attributes({"clan": "wolves", "level": 7, "old": null})
	await c.redeem("SUMMER-2026")
	await c.wallet()
	await c.spend("gems", 30, "sword-1")
	var replayed := ["/verify/google", "/entitlements", "/attributes"]
	var requests := []
	for call in calls:
		var request := {"route": call["path"]}
		if call["path"] in replayed:
			request["replay"] = true
		request["body"] = call["body"]
		requests.append(request)
	c.free()
	var path := ProjectSettings.globalize_path("res://").path_join("../../server/spacetimedb/sdk-requests/godot.json")
	if OS.get_environment("UPDATE_SDK_REQUESTS") != "":
		var out := FileAccess.open(path, FileAccess.WRITE)
		var note := "What the Godot addon (sdks/godot) sends to the server; its tests compare it (UPDATE_SDK_REQUESTS=1 rewrites it)."
		out.store_string(JSON.stringify({"note": note, "requests": requests}, "  ", false) + "\n")
		out.close()
	var fixture: Variant = JSON.parse_string(FileAccess.get_file_as_string(path))
	# Through JSON both ways: numbers are floats on both sides.
	var sent: Variant = JSON.parse_string(JSON.stringify(requests))
	check(fixture is Dictionary and fixture["requests"] == sent, "sdk-requests/godot.json is what the addon sends")
