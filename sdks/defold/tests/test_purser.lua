-- Tests of purser.lua with a stubbed http.request, on a stock Lua 5.1-5.4 or LuaJIT, from sdks/defold:
--   lua tests/test_purser.lua
-- UPDATE_SDK_REQUESTS=1 rewrites server/spacetimedb/sdk-requests/defold.json from what the module sends.

package.path = "./?.lua;./tests/?.lua;" .. package.path

local purser = require("purser.purser")
local json = require("json_decode")

local BASE = "https://stdb.example.com/v1/database/billing/route"
local failures, passed = 0, 0

local function deep_equal(a, b)
	if type(a) ~= type(b) then
		return false
	end
	if type(a) ~= "table" then
		return a == b
	end
	for k, v in pairs(a) do
		if not deep_equal(v, b[k]) then
			return false
		end
	end
	for k in pairs(b) do
		if a[k] == nil then
			return false
		end
	end
	return true
end

local function check(ok, what)
	if ok then
		passed = passed + 1
	else
		failures = failures + 1
		print("FAILED: " .. what)
	end
end

local function test(name, fn)
	local ok, err = pcall(fn)
	if not ok then
		failures = failures + 1
		print("ERROR in " .. name .. ": " .. tostring(err))
	end
end

--- A client whose http.request answers from `answers` (the last repeats) and records the calls.
local function client(answers, extra)
	local calls, sleeps = {}, {}
	local config = {
		base_url = BASE .. "/",
		app_id = "com.studio.game",
		user_id = "player-1",
		client = { device = "Pixel 8", os = "Android 15", app_version = "1.2.1" },
		json_decode = json.decode,
		delay = function(seconds, fn)
			sleeps[#sleeps + 1] = seconds
			fn()
		end,
		http_request = function(url, method, callback, headers, data, options)
			calls[#calls + 1] = { path = url:sub(#BASE + 1), method = method, headers = headers, body = json.decode(data), options = options }
			local a = answers[math.min(#calls, #answers)]
			local response = { status = a.status, headers = a.headers, error = a.error }
			if a.body ~= nil then
				response.response = type(a.body) == "string" and a.body or purser.json_encode(a.body)
			end
			callback(nil, "id", response)
		end,
	}
	for k, v in pairs(extra or {}) do
		config[k] = v
	end
	return purser.new(config), calls, sleeps
end

local function run(fn)
	local result
	fn(function(r)
		result = r
	end)
	return result
end

local success = { status = 200, body = { status = "success", transaction_id = "GPA.1", product_id = "gems", delivery = "server" } }

test("verify body", function()
	local c, calls = client({ success })
	local r = run(function(done)
		c:verify("google", { product_id = "gems", purchase_token = "tok-1", order_id = "", price_micros = 990000.4, currency = "USD" }, done)
	end)
	check(r.http_status == 200 and r.value.status == "success", "verify answer")
	check(calls[1].path == "/verify/google" and calls[1].method == "POST", "verify route")
	check(calls[1].headers["Content-Type"] == "application/json" and calls[1].headers["Authorization"] == nil, "headers")
	check(calls[1].options.timeout == 30, "timeout")
	check(deep_equal(calls[1].body, {
		app_id = "com.studio.game",
		user_id = "player-1",
		product_id = "gems",
		purchase_token = "tok-1",
		price_micros = 990000,
		currency = "USD",
		client = { device = "Pixel 8", os = "Android 15", app_version = "1.2.1", sdk_version = "defold/0.12.0" },
	}), "verify body")
end)

test("transport errors and 5xx are retried", function()
	local c, calls, sleeps = client({ { status = 0, error = "offline" }, { status = 502, body = { status = "error" } }, success })
	local r = run(function(done)
		c:entitlements(done)
	end)
	check(r.http_status == 200 and #calls == 3, "retried")
	check(deep_equal(sleeps, { 1, 2 }), "backoff")
end)

test("429 waits Retry-After, at most 60 s", function()
	local limited = { status = "error", code = "ERR_RATE_LIMITED" }
	local c, calls, sleeps = client({
		{ status = 429, body = limited, headers = { ["retry-after"] = "7" } },
		{ status = 429, body = limited, headers = { ["retry-after"] = "600" } },
		{ status = 429, body = limited },
		{ status = 200, body = { status = "ok", balances = {} } },
	})
	local r = run(function(done)
		c:wallet(done)
	end)
	check(r.http_status == 200 and #calls == 4, "retried after 429")
	check(deep_equal(sleeps, { 7, 60, 4 }), "Retry-After, then backoff without one")
end)

test("4xx is never retried", function()
	for _, status in ipairs({ 400, 403, 404, 409 }) do
		local c, calls, sleeps = client({ { status = status, body = { status = "invalid", code = "ERR_CODE_INVALID" } } })
		local r = run(function(done)
			c:redeem("SUMMER", done)
		end)
		check(#calls == 1 and #sleeps == 0, "HTTP " .. status .. " not retried")
		check(r.code == "ERR_CODE_INVALID" and r.error:find("HTTP " .. status, 1, true) == 1, "HTTP " .. status .. " error")
	end
end)

test("retries stop after the configured count", function()
	local c, calls = client({ { status = 503, body = { status = "error" } } }, { retries = 2 })
	local r = run(function(done)
		c:verify("apple", { product_id = "gems", order_id = "1" }, done)
	end)
	check(#calls == 3 and r.http_status == 503 and purser.classify(r) == purser.OUTCOME_RETRY, "gave up")
end)

test("player tokens", function()
	local issued = 0
	local token = function(user, done)
		issued = issued + 1
		done("token-" .. user .. "-" .. issued)
	end
	local refused = { status = 401, body = { status = "error", code = "ERR_CLIENT_AUTH" } }
	local c, calls = client({ success, refused, success, refused, refused }, { player_token = token })
	run(function(done)
		c:entitlements(done)
	end)
	check(calls[1].headers["Authorization"] == "Bearer token-player-1-1", "token sent")
	local r = run(function(done)
		c:entitlements(done)
	end)
	check(r.http_status == 200 and calls[2].headers["Authorization"] == "Bearer token-player-1-1", "old token refused")
	check(calls[3].headers["Authorization"] == "Bearer token-player-1-2", "new token")
	local again = run(function(done)
		c:entitlements(done)
	end)
	check(again.http_status == 401 and again.code == "ERR_CLIENT_AUTH" and #calls == 5, "refreshed once only")
	c:set_user_id("")
	run(function(done)
		c:paywall("store", done)
	end)
	check(calls[6].headers["Authorization"] == nil and calls[6].body.user_id == nil, "anonymous paywall")
end)

test("outcomes follow the Unity package", function()
	local cases = {
		{ 200, "success", purser.OUTCOME_SUCCESS, purser.FINISH },
		{ 202, "pending", purser.OUTCOME_PENDING_DELIVERY, purser.FINISH },
		{ 200, "pending_or_canceled", purser.OUTCOME_PENDING_PAYMENT, purser.KEEP },
		{ 400, "invalid", purser.OUTCOME_REJECTED, purser.FINISH_REFUSED },
		{ 403, "forbidden", purser.OUTCOME_REJECTED, purser.FINISH_REFUSED },
		{ 200, "expired", purser.OUTCOME_REJECTED, purser.FINISH_REFUSED },
		{ 200, "revoked", purser.OUTCOME_REJECTED, purser.FINISH_REFUSED },
		{ 400, "error", purser.OUTCOME_RETRY, purser.KEEP },
		{ 401, "error", purser.OUTCOME_RETRY, purser.KEEP },
		{ 502, "error", purser.OUTCOME_RETRY, purser.KEEP },
		{ 429, "error", purser.OUTCOME_RETRY, purser.KEEP },
		{ 0, nil, purser.OUTCOME_RETRY, purser.KEEP },
		{ 200, "something_new", purser.OUTCOME_RETRY, purser.KEEP },
	}
	for _, case in ipairs(cases) do
		local result = { http_status = case[1], value = case[2] and { status = case[2] } or nil }
		local outcome = purser.classify(result)
		check(outcome == case[3] and purser.finish_action(outcome) == case[4], "classify " .. case[1] .. " " .. tostring(case[2]))
	end
end)

test("process_purchase grants before finishing", function()
	local log = {}
	local handlers = {
		grant = function(answer)
			log[#log + 1] = "grant " .. answer.transaction_id
		end,
		finish = function(granted)
			log[#log + 1] = "finish " .. tostring(granted)
		end,
	}
	local client_grant = { status = 200, body = { status = "success", transaction_id = "GPA.1", delivery = "client" } }
	local c = client({ client_grant })
	local outcome, action
	c:process_purchase("google", { product_id = "gems", purchase_token = "tok-1" }, handlers, function(o, a)
		outcome, action = o, a
	end)
	check(outcome == purser.OUTCOME_SUCCESS and action == purser.FINISH, "success")
	check(deep_equal(log, { "grant GPA.1", "finish true" }), "grant, then finish")

	log = {}
	c:process_purchase("google", { product_id = "gems", purchase_token = "tok-1" }, {
		grant = function()
			return false
		end,
		finish = handlers.finish,
	}, function(_, a)
		action = a
	end)
	check(action == purser.KEEP and #log == 0, "a failed grant keeps the purchase")

	client({ success }):process_purchase("google", { product_id = "gems", purchase_token = "tok-1" }, handlers)
	check(deep_equal(log, { "finish true" }), "server delivery: no grant on the client")

	log = {}
	client({ { status = 403, body = { status = "forbidden" } } }):process_purchase("apple", { product_id = "gems", order_id = "1" }, handlers)
	client({ { status = 500, body = { status = "error" } } }, { retries = 0 }):process_purchase("apple", { product_id = "gems", order_id = "1" }, handlers)
	check(deep_equal(log, { "finish false" }), "refused: finished as refused; an error: kept")
end)

test("extension-iap transactions", function()
	local google = {
		ident = "gems",
		trans_ident = "GPA.1",
		receipt = "ignored",
		original_json = '{"orderId":"GPA.1","purchaseToken":"tok-1","productId":"gems"}',
	}
	check(deep_equal(purser.purchase_of(google, "google", json.decode), { product_id = "gems", order_id = "GPA.1", purchase_token = "tok-1" }), "google")
	check(deep_equal(purser.purchase_of({ ident = "gems", trans_ident = "2000000123" }, "apple"), { product_id = "gems", order_id = "2000000123" }), "apple")
	local amazon = purser.purchase_of({ ident = "gems", trans_ident = "receipt-1", user_id = "amzn1.account.TEST" }, "amazon")
	check(deep_equal(amazon, { product_id = "gems", order_id = "receipt-1", store_user_id = "amzn1.account.TEST" }), "amazon")
end)

test("json encoding", function()
	check(purser.json_encode({ b = 1, a = { "x", true }, c = 'q"\n' }) == '{"a":["x",true],"b":1,"c":"q\\"\\n"}', "sorted keys, escapes")
	check(purser.json_encode({ list = purser.EMPTY_ARRAY, obj = {} }) == '{"list":[],"obj":{}}', "empty array and object")
	check(purser.json_encode({ n = 1790000000000, f = 0.5 }) == '{"f":0.5,"n":1790000000000}', "numbers")
	check(purser.has_access({ server_time_ms = 10, access = { premium = { expires_at_ms = 0 } } }, "premium"), "lifetime access")
	check(not purser.has_access({ server_time_ms = 10, access = { premium = { expires_at_ms = 5 } } }, "premium"), "expired access")
end)

-- ---------------------------------------------------------------------------------------------
-- The contract: server/spacetimedb/sdk-requests/defold.json holds what the module sends; the
-- server's tests parse every body.
-- ---------------------------------------------------------------------------------------------

local function pretty(v, indent)
	indent = indent or ""
	if type(v) ~= "table" then
		return purser.json_encode(v)
	end
	local inner = indent .. "  "
	local parts = {}
	local n = 0
	for _ in pairs(v) do
		n = n + 1
	end
	if n == 0 then
		return getmetatable(v) == getmetatable(purser.EMPTY_ARRAY) and "[]" or "{}"
	end
	if #v == n then
		for i = 1, n do
			parts[i] = inner .. pretty(v[i], inner)
		end
		return "[\n" .. table.concat(parts, ",\n") .. "\n" .. indent .. "]"
	end
	local keys = {}
	for k in pairs(v) do
		keys[#keys + 1] = k
	end
	-- `route` and `replay` before `body`, like the other files.
	local order = { note = 1, requests = 2, route = 1, replay = 2, body = 3 }
	table.sort(keys, function(a, b)
		local oa, ob = order[a] or 9, order[b] or 9
		if oa ~= ob then
			return oa < ob
		end
		return a < b
	end)
	for i, k in ipairs(keys) do
		parts[i] = inner .. purser.json_encode(k) .. ": " .. pretty(v[k], inner)
	end
	return "{\n" .. table.concat(parts, ",\n") .. "\n" .. indent .. "}"
end

test("sdk-requests/defold.json is what the module sends", function()
	local c, calls = client({ { status = 200, body = { status = "ok" } } })
	local noop = function() end
	c:verify("google", {
		product_id = "gems",
		purchase_token = "tok-1",
		order_id = "GPA.1111-2222-3333-44444",
		price_micros = 990000,
		currency = "USD",
		paywall = { placement = "store", variant_id = "var-a", test_id = "test-1", paywall_id = "pw-1" },
		offer_id = "monthly-intro",
	}, noop)
	c:verify("apple", { product_id = "gems", order_id = "2000000123" }, noop)
	c:verify("amazon", { product_id = "gems", order_id = "receipt-1", store_user_id = "amzn1.account.TEST" }, noop)
	c:restore({ { product_id = "no_ads", purchase_token = "tok-old", order_id = "GPA.1" } }, noop)
	c:entitlements(noop)
	c:paywall("store", noop)
	c:paywall_event({ placement = "store", variant_id = "var-a", test_id = "test-1" }, noop)
	c:set_attributes({ clan = "wolves", level = 7, old = "" }, noop)
	c:redeem("SUMMER-2026", noop)
	c:wallet(noop)
	c:spend("gems", 30, "sword-1", noop)
	local replayed = { ["/verify/google"] = true, ["/entitlements"] = true, ["/attributes"] = true }
	local requests = {}
	for i, call in ipairs(calls) do
		requests[i] = { route = call.path, replay = replayed[call.path], body = call.body }
	end
	local path = "../../server/spacetimedb/sdk-requests/defold.json"
	if os.getenv("UPDATE_SDK_REQUESTS") then
		local note = "What the Defold module (sdks/defold) sends to the server; its tests compare it (UPDATE_SDK_REQUESTS=1 rewrites it)."
		local f = assert(io.open(path, "wb"))
		f:write(pretty({ note = note, requests = requests }), "\n")
		f:close()
	end
	local f = assert(io.open(path, "rb"))
	local fixture = json.decode(f:read("*a"))
	f:close()
	check(deep_equal(fixture.requests, requests), "defold.json differs: run UPDATE_SDK_REQUESTS=1 lua tests/test_purser.lua")
end)

print(string.format("%d checks passed, %d failed", passed, failures))
os.exit(failures == 0 and 0 or 1)
