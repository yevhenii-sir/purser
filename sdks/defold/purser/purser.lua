--- Client of a self-hosted Purser in-app purchase server for Defold: verify, restore, entitlements,
-- paywalls, promo codes, wallet. Uses http.request; every call takes a callback(result) with
--   result = { http_status = number (0: not reached), value = parsed body or nil, error = string or nil,
--              code = "ERR_..." or nil, retry_after = seconds of a 429 or nil }
-- See README.md.

local M = {}

M.SDK_VERSION = "0.13.0"
M.MAX_RESTORE_ITEMS = 20
--- A Retry-After longer than this (seconds) is cut to it.
M.MAX_RETRY_AFTER = 60
M.ERR_CLIENT_AUTH = "ERR_CLIENT_AUTH"

--- What a /verify answer means for the purchase (M.classify).
M.OUTCOME_SUCCESS = "success"
M.OUTCOME_PENDING_DELIVERY = "pending_delivery"
M.OUTCOME_PENDING_PAYMENT = "pending_payment"
M.OUTCOME_REJECTED = "rejected"
M.OUTCOME_RETRY = "retry"

--- What to do with the store purchase (M.finish_action).
M.FINISH = "finish"
M.FINISH_REFUSED = "finish_refused"
M.KEEP = "keep"

--- Encodes as an empty JSON array (an empty Lua table encodes as {}).
M.EMPTY_ARRAY = setmetatable({}, { __jsonarray = true })

-- ---------------------------------------------------------------------------------------------
-- JSON encoding (keys sorted; Defold's json.decode reads the answers)
-- ---------------------------------------------------------------------------------------------

local function is_array(t)
	if getmetatable(t) and getmetatable(t).__jsonarray then
		return true
	end
	local n = 0
	for _ in pairs(t) do
		n = n + 1
	end
	if n == 0 then
		return false
	end
	for i = 1, n do
		if t[i] == nil then
			return false
		end
	end
	return true
end

local escapes = { ['"'] = '\\"', ["\\"] = "\\\\", ["\b"] = "\\b", ["\f"] = "\\f", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }

local function encode_string(s)
	return '"' .. s:gsub('[%c"\\]', function(c)
		return escapes[c] or string.format("\\u%04x", c:byte())
	end) .. '"'
end

local function encode(v)
	local t = type(v)
	if t == "string" then
		return encode_string(v)
	elseif t == "number" then
		if v ~= v or v == math.huge or v == -math.huge then
			error("purser: cannot encode " .. tostring(v) .. " as JSON")
		end
		if v == math.floor(v) and math.abs(v) < 2 ^ 53 then
			return string.format("%d", v)
		end
		return string.format("%.17g", v)
	elseif t == "boolean" then
		return tostring(v)
	elseif t == "nil" then
		return "null"
	elseif t == "table" then
		local parts = {}
		if is_array(v) then
			for i = 1, #v do
				parts[i] = encode(v[i])
			end
			return "[" .. table.concat(parts, ",") .. "]"
		end
		local keys = {}
		for k in pairs(v) do
			keys[#keys + 1] = tostring(k)
		end
		table.sort(keys)
		for i, k in ipairs(keys) do
			parts[i] = encode_string(k) .. ":" .. encode(v[k])
		end
		return "{" .. table.concat(parts, ",") .. "}"
	end
	error("purser: cannot encode a " .. t .. " as JSON")
end

M.json_encode = encode

-- ---------------------------------------------------------------------------------------------
-- Purchase outcomes: the Unity package's rules
-- ---------------------------------------------------------------------------------------------

--- What a /verify answer means: M.OUTCOME_*.
function M.classify(result)
	if not result or (result.http_status or 0) == 0 or result.http_status == 429 then
		return M.OUTCOME_RETRY
	end
	local status = type(result.value) == "table" and result.value.status or nil
	if status == "success" then
		return M.OUTCOME_SUCCESS
	elseif status == "pending" then
		return M.OUTCOME_PENDING_DELIVERY
	elseif status == "pending_or_canceled" then
		return M.OUTCOME_PENDING_PAYMENT
	elseif status == "invalid" or status == "forbidden" or status == "expired" or status == "revoked" then
		return M.OUTCOME_REJECTED
	end
	-- `error` (server setup, store outage, refused player token), 5xx, an unexpected body.
	return M.OUTCOME_RETRY
end

--- M.FINISH: finish as granted (iap.finish). M.FINISH_REFUSED: finish as refused on Apple / Amazon; leave a Google
-- purchase unacknowledged (Google refunds it). M.KEEP: do not finish it, the store delivers it again.
function M.finish_action(outcome)
	if outcome == M.OUTCOME_SUCCESS or outcome == M.OUTCOME_PENDING_DELIVERY then
		return M.FINISH
	elseif outcome == M.OUTCOME_REJECTED then
		return M.FINISH_REFUSED
	end
	return M.KEEP
end

-- ---------------------------------------------------------------------------------------------
-- extension-iap glue
-- ---------------------------------------------------------------------------------------------

--- The Purser store of an extension-iap provider id (iap.get_provider_id()): "google", "apple", "amazon" or nil.
function M.store_of(provider_id)
	if not iap then
		return nil
	end
	if provider_id == iap.PROVIDER_ID_GOOGLE then
		return "google"
	elseif provider_id == iap.PROVIDER_ID_APPLE then
		return "apple"
	elseif provider_id == iap.PROVIDER_ID_AMAZON then
		return "amazon"
	end
	return nil
end

--- The /verify purchase of an extension-iap transaction (listener of iap.set_listener).
-- `decode` reads Google's original_json (default json.decode).
function M.purchase_of(transaction, store, decode)
	local p = { product_id = transaction.ident, order_id = transaction.trans_ident }
	if store == "google" then
		local ok, data = pcall(decode or (json and json.decode), transaction.original_json or "")
		if ok and type(data) == "table" then
			p.purchase_token = data.purchaseToken
			p.order_id = data.orderId or p.order_id
		end
		p.purchase_token = p.purchase_token or transaction.receipt
	elseif store == "amazon" then
		p.store_user_id = transaction.user_id
	end
	return p
end

-- ---------------------------------------------------------------------------------------------
-- Client
-- ---------------------------------------------------------------------------------------------

local Client = {}
Client.__index = Client

local function str(v)
	if v == nil then
		return ""
	end
	return tostring(v)
end

--- Sets `key` unless `v` is nil or empty (the server reads a missing field as its default).
local function put(body, key, v)
	if v ~= nil and v ~= "" then
		body[key] = tostring(v)
	end
end

local function default_client(app_version)
	local info = { device = "", os = "", app_version = app_version or "", sdk_version = "defold/" .. M.SDK_VERSION }
	if sys and sys.get_sys_info then
		local ok, s = pcall(sys.get_sys_info)
		if ok and s then
			info.device = str(s.device_model)
			info.os = (str(s.system_name) .. " " .. str(s.system_version)):match("^%s*(.-)%s*$")
		end
	end
	if info.app_version == "" and sys and sys.get_config_string then
		local ok, v = pcall(sys.get_config_string, "project.version", "")
		if ok then
			info.app_version = str(v)
		end
	end
	return info
end

--- A client of one build. `config`:
--   base_url        "https://<host>/v1/database/<db>/route"
--   app_id          this build's app_id
--   user_id         the player (or client:set_user_id later)
--   app_version     default game.project's project.version
--   client          { device =, os =, app_version = } to override what sys.get_sys_info says
--   player_token    builds with client_auth = signed: function(user_id, done) calling done(token) with a player
--                   token from the game's backend (nil: none). Kept until the server refuses it (401
--                   ERR_CLIENT_AUTH); then asked again and the request is sent again once.
--   timeout         seconds per request (default 30)
--   retries         retries after a transport error, a 5xx or a 429, never after another 4xx (default 3)
--   retry_delay     first retry delay in seconds, doubled each time (default 1); a 429 waits its Retry-After
--   http_request    default http.request (tests)
--   delay           function(seconds, fn), default timer.delay (tests)
--   json_decode     default json.decode (tests)
function M.new(config)
	assert(config and config.base_url and config.base_url ~= "", "purser: base_url is required")
	assert(config.app_id and config.app_id ~= "", "purser: app_id is required")
	local self = setmetatable({}, Client)
	self.base_url = config.base_url:gsub("/+$", "")
	self.app_id = config.app_id
	self.user_id = config.user_id or ""
	self.client = default_client(config.app_version)
	for _, key in ipairs({ "device", "os", "app_version" }) do
		if config.client and config.client[key] ~= nil then
			self.client[key] = str(config.client[key])
		end
	end
	self.player_token = config.player_token
	self.timeout = config.timeout or 30
	self.retries = config.retries or 3
	self.retry_delay = config.retry_delay or 1
	self.http_request = config.http_request or function(...)
		return http.request(...)
	end
	self.delay = config.delay or function(seconds, fn)
		timer.delay(seconds, false, function()
			fn()
		end)
	end
	self.json_decode = config.json_decode or function(s)
		return json.decode(s)
	end
	self.tokens = {}
	return self
end

--- The player of the following requests.
function Client:set_user_id(user_id)
	self.user_id = user_id or ""
end

local function failure(message)
	return { http_status = 0, error = message }
end

function Client:parse(response)
	local status = response.status or 0
	if status == 0 then
		return failure(response.error or "network error")
	end
	local text = response.response or ""
	local value, err
	if text ~= "" then
		local ok, decoded = pcall(self.json_decode, text)
		if ok then
			value = decoded
		else
			err = "invalid JSON from server: " .. tostring(decoded)
		end
	end
	local success = status >= 200 and status < 300
	local result = { http_status = status, value = value }
	if not success and type(value) == "table" and type(value.code) == "string" then
		result.code = value.code
	end
	if status == 429 and response.headers then
		local h = response.headers["retry-after"] or response.headers["Retry-After"]
		local seconds = tonumber(h)
		if seconds and seconds >= 0 then
			result.retry_after = math.min(seconds, M.MAX_RETRY_AFTER)
		end
	end
	if not err and not success then
		err = "HTTP " .. status .. ": " .. text:sub(1, 300)
	end
	result.error = err
	return result
end

--- POSTs `body` to `path` with the player token, retrying transport errors, 5xx and 429 (Retry-After), never
-- another 4xx; calls `callback(result)`. `for_user`: the player the token is for (default the client's player).
function Client:post(path, body, callback, for_user)
	local user = for_user or self.user_id
	local data = encode(body)
	local attempt, refreshed = 0, false
	local send

	local function with_token(fn)
		if not self.player_token or user == "" then
			return fn(nil)
		end
		if self.tokens[user] then
			return fn(self.tokens[user])
		end
		self.player_token(user, function(token)
			if token and token ~= "" then
				self.tokens[user] = token
			else
				token = nil
			end
			fn(token)
		end)
	end

	send = function()
		with_token(function(token)
			local headers = { ["Content-Type"] = "application/json" }
			if token then
				headers["Authorization"] = "Bearer " .. token
			end
			self.http_request(self.base_url .. path, "POST", function(_, _, response)
				local result = self:parse(response)
				local status = result.http_status
				if status == 401 and result.code == M.ERR_CLIENT_AUTH and self.player_token and user ~= "" and not refreshed then
					-- The token expired or was revoked: ask the game's backend once more.
					refreshed = true
					if self.tokens[user] == token then
						self.tokens[user] = nil
					end
					return send()
				end
				local retryable = status == 0 or status == 429 or status >= 500
				if not retryable or attempt >= self.retries then
					return callback(result)
				end
				local wait = self.retry_delay * 2 ^ attempt
				if status == 429 and result.retry_after then
					wait = result.retry_after
				end
				attempt = attempt + 1
				self.delay(wait, send)
			end, headers, data, { timeout = self.timeout })
		end)
	end
	send()
end

-- ---------------------------------------------------------------------------------------------
-- Purchases
-- ---------------------------------------------------------------------------------------------

--- The /verify body of a purchase: product_id, purchase_token (Google), order_id (Apple transactionId, Amazon
-- receiptId), store_user_id (Amazon userId), price_micros, currency, app_instance_id, attribution, paywall (a
-- /paywall answer), offer_id, client_finished.
function Client:verify_body(p)
	local body = { app_id = self.app_id, user_id = self.user_id, product_id = str(p.product_id) }
	for _, key in ipairs({ "purchase_token", "order_id", "store_user_id", "app_instance_id", "currency", "offer_id" }) do
		put(body, key, p[key])
	end
	body.price_micros = math.floor((p.price_micros or 0) + 0.5)
	if type(p.attribution) == "table" and next(p.attribution) then
		body.attribution = p.attribution
	end
	if type(p.paywall) == "table" then
		for _, key in ipairs({ "placement", "variant_id", "test_id", "paywall_id" }) do
			put(body, key, p.paywall[key])
		end
	end
	body.client = self.client
	if p.client_finished then
		body.client_finished = true
	end
	return body
end

--- POST /verify/<store> ("google", "apple", "amazon", ...).
function Client:verify(store, purchase, callback)
	self:post("/verify/" .. store, self:verify_body(purchase), callback)
end

--- Verifies a purchase and applies the Unity package's rules: on success / pending, handlers.grant(answer) runs
-- first (only when the server answers delivery = "client"; return false when granting failed: the purchase stays
-- unfinished), then handlers.finish(true); a refusal calls handlers.finish(false); anything else leaves the purchase
-- unfinished so the store delivers it again. callback(outcome, action, result) is optional.
function Client:process_purchase(store, purchase, handlers, callback)
	self:verify(store, purchase, function(result)
		M.apply_outcome(result, handlers, callback)
	end)
end

--- The rules of process_purchase for an answer already received.
function M.apply_outcome(result, handlers, callback)
	local outcome = M.classify(result)
	local action = M.finish_action(outcome)
	if action == M.FINISH then
		local v = result.value
		if v.delivery == "client" and handlers.grant and handlers.grant(v) == false then
			action = M.KEEP
		else
			handlers.finish(true)
		end
	elseif action == M.FINISH_REFUSED then
		handlers.finish(false)
	end
	if callback then
		callback(outcome, action, result)
	end
end

--- POST /restore: purchases = { { product_id =, purchase_token =, order_id =, store_user_id = }, ... } (at most 20);
-- options: store_user_id, app_instance_id, attribution, client_finished. Answer: { results, entitlements }.
function Client:restore(purchases, callback, options)
	options = options or {}
	if #purchases > M.MAX_RESTORE_ITEMS then
		return callback(failure("at most " .. M.MAX_RESTORE_ITEMS .. " purchases per restore call"))
	end
	local body = { app_id = self.app_id, user_id = self.user_id, client = self.client }
	put(body, "store_user_id", options.store_user_id)
	put(body, "app_instance_id", options.app_instance_id)
	if type(options.attribution) == "table" and next(options.attribution) then
		body.attribution = options.attribution
	end
	local items = {}
	for i, p in ipairs(purchases) do
		local item = { product_id = str(p.product_id) }
		put(item, "purchase_token", p.purchase_token)
		put(item, "order_id", p.order_id)
		put(item, "store_user_id", p.store_user_id)
		items[i] = item
	end
	body.purchases = #items > 0 and items or M.EMPTY_ARRAY
	if options.client_finished then
		body.client_finished = true
	end
	self:post("/restore", body, callback)
end

--- POST /entitlements: { access, subscriptions, non_consumables, paused?, server_time_ms }.
function Client:entitlements(callback)
	self:post("/entitlements", { app_id = self.app_id, user_id = self.user_id, client = self.client }, callback)
end

--- Whether an /entitlements answer gives `access` (e.g. "premium") now.
function M.has_access(customer_info, access)
	local state = type(customer_info) == "table" and type(customer_info.access) == "table" and customer_info.access[access]
	if type(state) ~= "table" then
		return false
	end
	local expires = state.expires_at_ms or 0
	return expires == 0 or expires > (customer_info.server_time_ms or 0)
end

-- ---------------------------------------------------------------------------------------------
-- Paywalls, attributes, promo codes, wallet
-- ---------------------------------------------------------------------------------------------

--- POST /paywall: what to show at `placement`.
function Client:paywall(placement, callback)
	local body = { app_id = self.app_id, placement = placement, client = self.client }
	put(body, "user_id", self.user_id)
	self:post("/paywall", body, callback)
end

--- POST /paywall/event: the /paywall answer `shown` was shown.
function Client:paywall_event(shown, callback, event)
	local body = { app_id = self.app_id, placement = str(shown.placement), variant_id = str(shown.variant_id), event = event or "view" }
	put(body, "test_id", shown.test_id)
	put(body, "user_id", self.user_id)
	self:post("/paywall/event", body, callback)
end

--- POST /attributes: strings, numbers, booleans; "" removes a key.
function Client:set_attributes(attributes, callback)
	self:post("/attributes", { app_id = self.app_id, user_id = self.user_id, attributes = attributes, client = self.client }, callback)
end

--- POST /redeem: a promo code; the answer has the shape of /verify.
function Client:redeem(code, callback)
	self:post("/redeem", { app_id = self.app_id, user_id = self.user_id, code = code, client = self.client }, callback)
end

--- POST /wallet: { status = "ok", balances = { gems = 120 } }.
function Client:wallet(callback)
	self:post("/wallet", { app_id = self.app_id, user_id = self.user_id, client = self.client }, callback)
end

--- POST /wallet/spend (builds with player tokens). Retry a failed spend with the same `reference`.
function Client:spend(currency, amount, reference, callback, note)
	local body = { app_id = self.app_id, user_id = self.user_id, currency = currency, amount = math.floor(amount + 0.5), reference = reference, client = self.client }
	put(body, "note", note)
	self:post("/wallet/spend", body, callback)
end

--- Google obfuscatedAccountId: the player id. Ids longer than 64 characters are hashed (SHA-256) by the server, which
-- Defold has no built-in for: keep player ids at 64 characters or less, or hash them with extension-crypt.
function M.google_account_id(user_id)
	assert(#user_id <= 64, "purser: ids longer than 64 characters need SHA-256 (extension-crypt)")
	return user_id
end

return M
