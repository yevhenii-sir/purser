-- A small JSON decoder for the tests (Defold has json.decode; a stock Lua has none).
-- JSON null decodes to the `null` sentinel of this module.

local M = { null = setmetatable({}, { __tostring = function() return "null" end }) }

local function fail(s, i, what)
	error(string.format("JSON: %s at %d near %q", what, i, s:sub(i, i + 10)))
end

local function skip(s, i)
	return s:find("[^ \t\r\n]", i) or #s + 1
end

local decode_value

local function decode_string(s, i)
	local out, j = {}, i + 1
	while true do
		local c = s:sub(j, j)
		if c == "" then
			fail(s, i, "unterminated string")
		elseif c == '"' then
			return table.concat(out), j + 1
		elseif c == "\\" then
			local e = s:sub(j + 1, j + 1)
			local map = { b = "\b", f = "\f", n = "\n", r = "\r", t = "\t", ['"'] = '"', ["\\"] = "\\", ["/"] = "/" }
			if e == "u" then
				local code = tonumber(s:sub(j + 2, j + 5), 16)
				if code < 0x80 then
					out[#out + 1] = string.char(code)
				elseif code < 0x800 then
					out[#out + 1] = string.char(0xC0 + math.floor(code / 0x40), 0x80 + code % 0x40)
				else
					out[#out + 1] = string.char(0xE0 + math.floor(code / 0x1000), 0x80 + math.floor(code / 0x40) % 0x40, 0x80 + code % 0x40)
				end
				j = j + 6
			else
				out[#out + 1] = map[e] or fail(s, j, "bad escape")
				j = j + 2
			end
		else
			out[#out + 1] = c
			j = j + 1
		end
	end
end

decode_value = function(s, i)
	i = skip(s, i)
	local c = s:sub(i, i)
	if c == "{" then
		local obj = {}
		i = skip(s, i + 1)
		if s:sub(i, i) == "}" then
			return obj, i + 1
		end
		while true do
			local key
			key, i = decode_string(s, skip(s, i))
			i = skip(s, i)
			if s:sub(i, i) ~= ":" then
				fail(s, i, "expected ':'")
			end
			obj[key], i = decode_value(s, i + 1)
			i = skip(s, i)
			local d = s:sub(i, i)
			if d == "}" then
				return obj, i + 1
			elseif d ~= "," then
				fail(s, i, "expected ',' or '}'")
			end
			i = i + 1
		end
	elseif c == "[" then
		local arr = {}
		i = skip(s, i + 1)
		if s:sub(i, i) == "]" then
			return arr, i + 1
		end
		while true do
			arr[#arr + 1], i = decode_value(s, i)
			i = skip(s, i)
			local d = s:sub(i, i)
			if d == "]" then
				return arr, i + 1
			elseif d ~= "," then
				fail(s, i, "expected ',' or ']'")
			end
			i = i + 1
		end
	elseif c == '"' then
		return decode_string(s, i)
	elseif s:sub(i, i + 3) == "true" then
		return true, i + 4
	elseif s:sub(i, i + 4) == "false" then
		return false, i + 5
	elseif s:sub(i, i + 3) == "null" then
		return M.null, i + 4
	end
	local num = s:match("^-?%d+%.?%d*[eE]?[-+]?%d*", i)
	if not num or num == "" then
		fail(s, i, "unexpected character")
	end
	return tonumber(num), i + #num
end

function M.decode(s)
	local v, i = decode_value(s, 1)
	if skip(s, i) <= #s then
		fail(s, i, "trailing characters")
	end
	return v
end

return M
