"""MCP server for an purser database: lets Claude (or any MCP client) read revenue, players,
purchases, setup checks, integration health, paywall results and logs.

Stdlib only: JSON-RPC 2.0 over stdio, one message per line (the MCP stdio transport).

Usage (register it with your MCP client, e.g. Claude Code):
    claude mcp add purser -- python server/mcp_server.py --database <your-billing-db>
    python mcp_server.py --server local --database billing-local
    python mcp_server.py --allow-writes      # also grant_manual, revoke_purchase, requeue_purchase

Server and database default to server/spacetime.json, like seed.py. The token is `PURSER_TOKEN` when
set, otherwise the one of `spacetime login`; its identity must be an admin of the database (the owner
for list_apps / integration_health / paywall_results without a placement, which read tables).

Everything goes through the module's admin procedures and a few fixed SQL queries; there is no
free-form SQL tool and secrets are never read. Player ids and purchases a tool returns are sent to the
model provider of your MCP client.
"""
import argparse
import datetime
import json
import pathlib
import sys
import time
import traceback

import stdb

ROOT = pathlib.Path(__file__).resolve().parent
SERVER_NAME = "purser"
SERVER_VERSION = "1.0.0"
PROTOCOL_VERSIONS = ["2025-06-18", "2025-03-26", "2024-11-05"]
# SpacetimeDB logs this for every procedure called through /call (it looks for a reducer first).
LOG_NOISE = "External attempt to call nonexistent reducer"


class ToolError(Exception):
    """A tool failure reported to the model as an `isError` result."""


# ---------------------------------------------------------------------------------------------
# Database access
# ---------------------------------------------------------------------------------------------

class Db:
    def __init__(self, server, database):
        self.server, self.database = server, database
        self._base = self._tok = None

    def _conn(self):
        if self._base is None:
            self._base = stdb.server_url_raw(self.server)
        if self._tok is None:
            self._tok = stdb.token_raw()
        return self._base, self._tok

    def sql(self, query):
        base, tok = self._conn()
        return stdb.select_raw(base, self.database, tok, query)

    def call(self, name, *args):
        """A reducer (returns None) or an admin procedure (its JSON; `{"error"}` raises)."""
        base, tok = self._conn()
        try:
            value = stdb.call_raw(base, self.database, tok, name, *args)
        except stdb.StdbError as e:
            if e.status == 530 and e.body:  # the reducer returned an error
                raise ToolError(f"{name}: {e.body.strip()}")
            raise
        if isinstance(value, dict) and "error" in value:
            raise ToolError(f"{name}: {value['error']}")
        return value

    def logs(self, n):
        base, tok = self._conn()
        return stdb.logs(base, self.database, tok, n)


# ---------------------------------------------------------------------------------------------
# Argument helpers
# ---------------------------------------------------------------------------------------------

def arg_str(args, name, required=False, default=""):
    v = args.get(name, default)
    if v is None:
        v = default
    if not isinstance(v, (str, int, float)) or isinstance(v, bool):
        raise ToolError(f"'{name}' must be a string")
    v = str(v).strip()
    if required and not v:
        raise ToolError(f"'{name}' is required")
    return v


def arg_bool(args, name, default=False):
    v = args.get(name, default)
    if isinstance(v, bool):
        return v
    if isinstance(v, str) and v.lower() in ("true", "false"):
        return v.lower() == "true"
    raise ToolError(f"'{name}' must be true or false")


def arg_int(args, name, default, lo, hi):
    v = args.get(name, default)
    try:
        v = int(v)
    except (TypeError, ValueError):
        raise ToolError(f"'{name}' must be a number")
    return max(lo, min(hi, v))


def arg_list(args, name):
    v = args.get(name) or []
    if isinstance(v, str):
        v = [v]
    if not isinstance(v, list) or not all(isinstance(x, str) for x in v):
        raise ToolError(f"'{name}' must be a list of strings")
    return [x.strip() for x in v if x.strip()]


def parse_day(s, name):
    try:
        return datetime.date.fromisoformat(s)
    except ValueError:
        raise ToolError(f"'{name}' must be a date YYYY-MM-DD")


def time_micros(s, name, end_of_day=False):
    """`YYYY-MM-DD` (UTC; the end of the day when `end_of_day`) or an ISO 8601 time -> micros."""
    if len(s) == 10:
        d = parse_day(s, name)
        t = datetime.datetime.combine(d, datetime.time.max if end_of_day else datetime.time.min, datetime.timezone.utc)
    else:
        try:
            t = datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))
        except ValueError:
            raise ToolError(f"'{name}' must be YYYY-MM-DD or an ISO 8601 time")
        if t.tzinfo is None:
            t = t.replace(tzinfo=datetime.timezone.utc)
    return int(t.timestamp() * 1_000_000)


def iso_micros(us):
    if not us:
        return None
    return datetime.datetime.fromtimestamp(us / 1_000_000, datetime.timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def app_ids_of_game(db, game_key):
    rows = db.sql(f"SELECT app_id FROM game_app WHERE game_key = {stdb.sql_str(game_key)}")
    if not rows:
        raise ToolError(f"no builds with game_key '{game_key}' (see list_apps)")
    return [r["app_id"] for r in rows]


def scope_app_ids(db, args):
    """Build ids from `app_id` / `app_ids` / `game_key`; empty = all."""
    ids = arg_list(args, "app_ids")
    one = arg_str(args, "app_id")
    if one:
        ids.append(one)
    game_key = arg_str(args, "game_key")
    if game_key:
        ids.extend(app_ids_of_game(db, game_key))
    return sorted(set(ids))


# ---------------------------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------------------------

def t_list_apps(db, args):
    game_key = arg_str(args, "game_key")
    where = f" WHERE game_key = {stdb.sql_str(game_key)}" if game_key else ""
    apps = db.sql("SELECT * FROM game_app" + where)
    products = db.sql("SELECT * FROM product")
    by_app = {}
    for p in products:
        by_app.setdefault(p["app_id"], []).append({
            "product_id": p["product_id"], "kind": p["kind"],
            "default_price": p["default_price_micros"] / 1_000_000 if p["default_price_micros"] else None,
            "currency": p["currency"] or None,
        })
    games = {}
    for a in sorted(apps, key=lambda a: (a["game_key"], a["app_id"])):
        games.setdefault(a["game_key"], []).append({
            "app_id": a["app_id"], "platform": a["platform"], "store": a.get("store") or "(platform default)",
            "account": a["account"], "delivery": a["delivery"], "enabled": a["enabled"],
            "allow_sandbox": a["allow_sandbox"],
            "products": sorted(by_app.get(a["app_id"], []), key=lambda p: p["product_id"]),
        })
    try:
        names = {r["game_key"]: r["display_name"] for r in db.sql("SELECT * FROM app_profile")}
    except stdb.StdbError:  # a module older than app_profile
        names = {}
    return {"apps": [{"game_key": k, "name": names.get(k, k), "builds": v} for k, v in games.items()]}


def t_overview(db, args):
    return db.call("admin_overview")


def t_revenue(db, args):
    q = {"summary": True, "include_sandbox": arg_bool(args, "include_sandbox")}
    for k in ("from", "to"):
        v = arg_str(args, k)
        if v:
            parse_day(v, k)
            q[k] = v
    game_key = arg_str(args, "game_key")
    if game_key:
        q["game_key"] = game_key
    ids = arg_list(args, "app_ids")
    if ids:
        q["app_ids"] = ids
    return db.call("admin_revenue_stats", json.dumps(q))


def t_find_player(db, args):
    return db.call("admin_find_player", arg_str(args, "query", required=True))


def t_player_profile(db, args):
    return db.call("admin_user_profile", arg_str(args, "user_id", required=True))


def t_list_purchases(db, args):
    q = {"limit": arg_int(args, "limit", 50, 1, 1000)}
    since, until = arg_str(args, "since"), arg_str(args, "until")
    q["since_micros"] = time_micros(since, "since") if since else 0
    if until:
        q["until_micros"] = time_micros(until, "until", end_of_day=True)
    ids = scope_app_ids(db, args)
    if ids:
        q["app_ids"] = ids
    status = arg_str(args, "status")
    if status:
        q["status"] = status
    res = db.call("admin_list_purchases", json.dumps(q))
    for r in res.get("rows", []):
        for k in ("created_at_micros", "updated_at_micros", "next_attempt_at_micros"):
            if r.get(k):
                r[k.replace("_micros", "")] = iso_micros(r[k])
    return res


def t_check_app(db, args):
    ids = scope_app_ids(db, args)
    if not ids:
        raise ToolError("pass app_id or game_key (see list_apps)")
    offline = arg_bool(args, "offline")
    options = json.dumps({"online": not offline, "apple_test": False})
    reports = []
    for app_id in ids:
        rep = db.call("admin_check_app", app_id, options)
        reports.append({
            "app_id": app_id, "summary": rep.get("summary"),
            "checks": [{k: c.get(k) for k in ("level", "title", "detail", "fix") if c.get(k)}
                       for c in rep.get("checks", []) if args.get("verbose") or c.get("level") != "ok"],
            "urls": rep.get("urls"),
        })
    return {"reports": reports, "note": "checks with level ok are left out unless verbose is true"}


def t_integration_health(db, args):
    ids = set(scope_app_ids(db, args))
    keep = (lambda app_id: app_id in ids) if ids else (lambda app_id: True)
    integrations = [i for i in db.sql("SELECT app_id, kind, enabled FROM integration") if keep(i["app_id"])]
    queue = {}
    for o in db.sql("SELECT kind, app_id, attempts FROM integration_outbox"):
        if keep(o["app_id"]):
            q = queue.setdefault((o["app_id"], o["kind"]), {"queued": 0, "retrying": 0, "max_attempts": 0})
            q["queued"] += 1
            q["retrying"] += 1 if o["attempts"] > 0 else 0
            q["max_attempts"] = max(q["max_attempts"], o["attempts"])
    alerts = {}
    for a in db.sql("SELECT * FROM alert_state WHERE rule = 'integration_failed'"):
        if keep(a["app_id"]):
            alerts[a["app_id"]] = {
                "active": a["active"], "last_at": iso_micros(a["last_at_micros"]), "total": a["total"],
                "detail": a["detail"],
            }
    out = []
    for i in sorted(integrations, key=lambda i: (i["app_id"], i["kind"])):
        row = {"app_id": i["app_id"], "kind": i["kind"], "enabled": i["enabled"],
               **queue.pop((i["app_id"], i["kind"]), {"queued": 0, "retrying": 0, "max_attempts": 0})}
        out.append(row)
    orphans = [{"app_id": a, "kind": k, **v} for (a, k), v in queue.items()]
    return {
        "integrations": out,
        "queued_for_removed_integrations": orphans,
        "last_failures_by_app": alerts,
        "note": "queued = events waiting in the outbox, retrying = those that failed at least once; "
                "failures are what the integration_failed alert rule saw (dropped events)",
    }


def t_paywall_results(db, args):
    game_key = arg_str(args, "game_key", required=True)
    placement = arg_str(args, "placement_id")
    if not placement:
        placements = db.sql(f"SELECT * FROM placement WHERE game_key = {stdb.sql_str(game_key)}")
        tests = db.sql("SELECT * FROM paywall_test")
        out = []
        for p in sorted(placements, key=lambda p: p["placement_id"]):
            out.append({
                "placement_id": p["placement_id"], "enabled": p["enabled"],
                "variants": json.loads(p["variants_json"] or "[]"),
                "running_test": p["test_id"] if p["test_id"] and not p["test_stopped_at_micros"] else None,
                "tests": [{"test_id": t["test_id"], "started_at": iso_micros(t["started_at_micros"]),
                           "stopped_at": iso_micros(t["stopped_at_micros"]),
                           "winner_variant_id": t["winner_variant_id"] or None}
                          for t in sorted(tests, key=lambda t: t["started_at_micros"]) if t["placement_key"] == p["key"]],
            })
        return {"game_key": game_key, "placements": out,
                "note": "pass placement_id (and test_id, or from/to) for the results"}
    q = {"game_key": game_key, "placement_id": placement, "include_sandbox": arg_bool(args, "include_sandbox")}
    test_id = arg_str(args, "test_id")
    if test_id:
        q["test_id"] = test_id
    for k in ("from", "to"):
        v = arg_str(args, k)
        if v:
            parse_day(v, k)
            q[k] = v
    return db.call("admin_paywall_results", json.dumps(q))


def t_recent_logs(db, args):
    n = arg_int(args, "lines", 100, 1, 1000)
    needle = arg_str(args, "filter").lower()
    level = arg_str(args, "level").lower()
    # Read more than asked: procedure-call noise is dropped.
    raw = db.logs(min(n * 3, 3000))
    out = []
    for l in raw:
        msg = str(l.get("message", ""))
        if LOG_NOISE in msg:
            continue
        if level and str(l.get("level", "")).lower() != level:
            continue
        if needle and needle not in json.dumps(l).lower():
            continue
        ts = l.get("ts")
        if isinstance(ts, (int, float)):
            ts = iso_micros(ts)
        out.append({"ts": ts, "level": l.get("level"), "message": msg})
    return {"lines": out[-n:], "note": "oldest first; procedure-call notices are left out"}


def t_grant_manual(db, args):
    app_id = arg_str(args, "app_id", required=True)
    user_id = arg_str(args, "user_id", required=True)
    product_id = arg_str(args, "product_id", required=True)
    tx_id = arg_str(args, "transaction_id") or f"mcp-{int(time.time() * 1000)}"
    expires = arg_str(args, "expires_at")
    body = {
        "app_id": app_id, "user_id": user_id, "product_id": product_id, "transaction_id": tx_id,
        "expires_at_ms": time_micros(expires, "expires_at", end_of_day=True) // 1000 if expires else 0,
        "note": arg_str(args, "note"),
    }
    db.call("admin_grant_manual", json.dumps(body))
    return {"ok": True, "purchase_id": f"manual:{tx_id}",
            "note": "the worker delivers it to the game; see player_profile for the status"}


def t_revoke_purchase(db, args):
    return db.call("admin_revoke_purchase", arg_str(args, "purchase_id", required=True), arg_str(args, "reason"))


def t_requeue_purchase(db, args):
    pid = arg_str(args, "purchase_id", required=True)
    db.call("admin_requeue_purchase", pid)
    return {"ok": True, "purchase_id": pid, "note": "the worker retries it on its next run"}


S = {"type": "string"}
B = {"type": "boolean"}
DAY = {"type": "string", "description": "YYYY-MM-DD (UTC)"}
SCOPE = {
    "app_id": {"type": "string", "description": "One build (see list_apps)"},
    "game_key": {"type": "string", "description": "All builds of one app (see list_apps)"},
}


def schema(props=None, required=()):
    return {"type": "object", "properties": props or {}, "required": list(required), "additionalProperties": False}


READ = {"readOnlyHint": True, "openWorldHint": False}
# admin_check_app stores its report in `app_check` and, online, calls the stores, the delivery target
# and integration endpoints.
CHECK = {"readOnlyHint": False, "destructiveHint": False, "idempotentHint": False, "openWorldHint": True}

TOOLS = [
    ("list_apps", t_list_apps, READ,
     "Apps (game_key) with their builds (one per store: app_id, platform, store, account, delivery) and "
     "each build's product catalog. Start here to learn the ids other tools take.",
     schema({"game_key": {"type": "string", "description": "Only this app"}})),
    ("overview", t_overview, READ,
     "Ledger counters: purchases in total and by status, purchases the worker is still processing "
     "(with their last error), number of apps, products and queued integration events.",
     schema()),
    ("revenue", t_revenue, READ,
     "Revenue summary in USD from the daily aggregates: totals (gross, refunds, net, proceeds after the "
     "store fee, purchases, payers, trials, renewals), the daily series, by app, by product, "
     "subscriptions and paywall views. Default range: the last 30 days; sandbox purchases are left out.",
     schema({"from": DAY, "to": DAY, "game_key": SCOPE["game_key"],
             "app_ids": {"type": "array", "items": S, "description": "Only these builds"},
             "include_sandbox": B})),
    ("find_player", t_find_player, READ,
     "Finds the player(s) behind a user id, ledger id ({store}:{transaction_id}), transaction / order id "
     "or Google purchase token.",
     schema({"query": S}, ["query"])),
    ("player_profile", t_player_profile, READ,
     "One player: builds seen, spending, entitlements per game (as /entitlements returns them), the "
     "purchase timeline with statuses and delivery errors, attribution ids.",
     schema({"user_id": S}, ["user_id"])),
    ("list_purchases", t_list_purchases, READ,
     "Ledger rows updated in a time range, newest first. Statuses: verified, granted, completed, "
     "revoke_pending, revoked, expire_pending, expired, transfer_pending, rejected.",
     schema({**SCOPE, "app_ids": {"type": "array", "items": S},
             "status": S,
             "since": {"type": "string", "description": "YYYY-MM-DD or ISO 8601 time (default: all time)"},
             "until": {"type": "string", "description": "YYYY-MM-DD (inclusive) or ISO 8601 time"},
             "limit": {"type": "integer", "description": "1-1000, default 50"}})),
    ("check_app", t_check_app, CHECK,
     "Runs the setup check of a build (or every build of an app): store keys, notifications, delivery, "
     "integrations, with fix hints. Online by default (calls the store APIs and the game's delivery "
     "target); offline only checks the configuration. Never asks Apple for a test notification.",
     schema({**SCOPE, "offline": B, "verbose": {"type": "boolean", "description": "Include passed checks"}})),
    ("integration_health", t_integration_health, READ,
     "Outgoing integrations (GA4, AppsFlyer, Adjust, Tenjin, AppMetrica, webhook, Cloud Storage) per "
     "build: enabled, events waiting or retrying in the outbox, and the last dropped-event failures.",
     schema(SCOPE)),
    ("paywall_results", t_paywall_results, READ,
     "Paywall A/B results of a placement: views, purchases, conversion, USD revenue, ARPU per view and "
     "probability to be best per variant. Without placement_id: the app's placements and their tests.",
     schema({"game_key": S, "placement_id": S, "test_id": S, "from": DAY, "to": DAY, "include_sandbox": B},
            ["game_key"])),
    ("recent_logs", t_recent_logs, READ,
     "The last lines of the module log (errors of deliveries, store calls, notifications, the worker).",
     schema({"lines": {"type": "integer", "description": "1-1000, default 100"},
             "filter": {"type": "string", "description": "Substring to keep"},
             "level": {"type": "string", "description": "Only this level: error, warn, info, debug"}})),
]

WRITE_TOOLS = [
    ("grant_manual", t_grant_manual, {"readOnlyHint": False, "destructiveHint": False, "idempotentHint": False},
     "Grants a product to a player without a store purchase (support compensation, promo). It goes "
     "through the ledger: the worker delivers it to the game and /entitlements lists it. Pass a "
     "transaction_id to make retries safe.",
     schema({"app_id": S, "user_id": S, "product_id": S, "transaction_id": S,
             "expires_at": {"type": "string", "description": "Subscriptions: YYYY-MM-DD or ISO time; empty = never"},
             "note": S}, ["app_id", "user_id", "product_id"])),
    ("revoke_purchase", t_revoke_purchase, {"readOnlyHint": False, "destructiveHint": True, "idempotentHint": True},
     "Revokes a purchase the player owns: the game gets a revoke (reason admin) and /entitlements drops "
     "it. Not a refund: no money moves, revenue is not changed.",
     schema({"purchase_id": {"type": "string", "description": "Ledger id {store}:{transaction_id}"},
             "reason": S}, ["purchase_id"])),
    ("requeue_purchase", t_requeue_purchase, {"readOnlyHint": False, "destructiveHint": False, "idempotentHint": True},
     "Makes the worker retry a purchase's unfinished step (delivery, consume, revoke) on its next run.",
     schema({"purchase_id": S}, ["purchase_id"])),
]


# ---------------------------------------------------------------------------------------------
# JSON-RPC
# ---------------------------------------------------------------------------------------------

class Server:
    def __init__(self, db, allow_writes):
        self.db = db
        tools = TOOLS + (WRITE_TOOLS if allow_writes else [])
        self.tools = {name: (fn, ann, desc, sch) for name, fn, ann, desc, sch in tools}

    def tool_list(self):
        return [{"name": name, "description": desc, "inputSchema": sch, "annotations": ann}
                for name, (fn, ann, desc, sch) in self.tools.items()]

    def call_tool(self, params):
        name = params.get("name")
        args = params.get("arguments") or {}
        if name not in self.tools:
            hint = " (start the server with --allow-writes)" if name in {t[0] for t in WRITE_TOOLS} else ""
            return error_result(f"unknown tool '{name}'{hint}")
        if not isinstance(args, dict):
            return error_result("arguments must be an object")
        try:
            value = self.tools[name][0](self.db, args)
        except (ToolError, stdb.StdbError) as e:
            return error_result(str(e))
        except Exception as e:  # never let one call take the server down
            traceback.print_exc(file=sys.stderr)
            return error_result(f"internal error: {type(e).__name__}: {e}")
        return {"content": [{"type": "text", "text": json.dumps(value, indent=1, ensure_ascii=False)}], "isError": False}

    def handle(self, msg):
        """One request or notification -> the response dict, or None for notifications."""
        if not isinstance(msg, dict) or msg.get("jsonrpc") != "2.0" or not isinstance(msg.get("method"), str):
            return rpc_error(msg.get("id") if isinstance(msg, dict) else None, -32600, "Invalid Request")
        method, mid, params = msg["method"], msg.get("id"), msg.get("params")
        if "id" not in msg:
            return None  # notifications/initialized, notifications/cancelled, ...
        if params is None:
            params = {}
        if not isinstance(params, dict):
            return rpc_error(mid, -32602, "params must be an object")
        if method == "initialize":
            requested = params.get("protocolVersion")
            return rpc_result(mid, {
                "protocolVersion": requested if requested in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0],
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "instructions": f"Billing database '{self.db.database}'. Call list_apps first to learn the app ids. "
                                "Amounts are USD unless a currency is given; micros fields are millionths.",
            })
        if method == "ping":
            return rpc_result(mid, {})
        if method == "tools/list":
            return rpc_result(mid, {"tools": self.tool_list()})
        if method == "tools/call":
            if not isinstance(params, dict) or not isinstance(params.get("name"), str):
                return rpc_error(mid, -32602, "params.name is required")
            return rpc_result(mid, self.call_tool(params))
        if method in ("resources/list", "prompts/list"):
            return rpc_result(mid, {method.split("/")[0]: []})
        return rpc_error(mid, -32601, f"Method not found: {method}")


def error_result(text):
    return {"content": [{"type": "text", "text": text}], "isError": True}


def rpc_result(mid, result):
    return {"jsonrpc": "2.0", "id": mid, "result": result}


def rpc_error(mid, code, message):
    return {"jsonrpc": "2.0", "id": mid, "error": {"code": code, "message": message}}


def handle_safely(server, msg):
    """`server.handle`, with any unexpected failure answered as an internal error."""
    try:
        return server.handle(msg)
    except Exception:
        traceback.print_exc(file=sys.stderr)
        mid = msg.get("id") if isinstance(msg, dict) else None
        if isinstance(msg, dict) and "id" not in msg:
            return None
        return rpc_error(mid, -32603, "Internal error")


def serve(server, inp, out):
    for line in inp:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            reply = rpc_error(None, -32700, "Parse error")
        else:
            if isinstance(msg, list):  # JSON-RPC batch (older protocol versions)
                reply = [r for r in (handle_safely(server, m) for m in msg) if r is not None] or None
            else:
                reply = handle_safely(server, msg)
        if reply is not None:
            out.write(json.dumps(reply) + "\n")
            out.flush()


def main():
    for s in (sys.stdin, sys.stdout):
        if hasattr(s, "reconfigure"):
            s.reconfigure(encoding="utf-8")
    cfg_path = ROOT / "spacetime.json"
    cfg = json.loads(cfg_path.read_text(encoding="utf-8")) if cfg_path.exists() else {}
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--server", default=cfg.get("server", "maincloud"), help="CLI server nickname or URL")
    p.add_argument("--database", default=cfg.get("database"))
    p.add_argument("--allow-writes", action="store_true",
                   help="also offer grant_manual, revoke_purchase and requeue_purchase")
    a = p.parse_args()
    if not a.database or a.database.startswith("<"):
        sys.exit("No database: pass --database or copy spacetime.example.json to spacetime.json and set it")
    print(f"{SERVER_NAME}: {a.server}/{a.database}, writes {'on' if a.allow_writes else 'off'}", file=sys.stderr)
    serve(Server(Db(a.server, a.database), a.allow_writes), sys.stdin, sys.stdout)


if __name__ == "__main__":
    main()
