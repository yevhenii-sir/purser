# MCP server (ask Claude about your billing)

`server/mcp_server.py` is a [Model Context Protocol](https://modelcontextprotocol.io) server for a billing database. With it Claude Code, Claude Desktop or any other MCP client can answer questions such as "how much did we make last week by app?", "why didn't player 12345 get their gems?" or "is the iOS build set up correctly?" from your own database.

It is one Python file with no dependencies (Python 3.9+, standard library only). It talks to the module over the same HTTP API as the admin page and only calls the admin procedures and a few fixed read queries: there is no free-form SQL tool, and secrets are never read.

## Register it

From the repository root, signed in with `spacetime login` as the database owner (or an admin):

```bash
claude mcp add purser -- python server/mcp_server.py --database <your-billing-db>
```

Options:

| Option | |
|---|---|
| `--server` | CLI server nickname (`maincloud`, `local`, ...) or a URL such as `http://127.0.0.1:3000` (a self-hosted server through the SSH tunnel). Default: `server/spacetime.json`, else `maincloud` |
| `--database` | Database name. Default: `server/spacetime.json` |
| `--allow-writes` | Also offer the write tools below. Off by default |

The token is the `PURSER_TOKEN` environment variable when it is set, otherwise the one `spacetime login show --token` prints. It is sent only over https, or over http to this machine (`localhost`, `127.0.0.1`, `[::1]`), and is dropped from redirects to another origin. Its identity must be an admin of the database. The tools that read tables (`list_apps`, `integration_health`, `paywall_results` without a placement) need the database **owner**, because all tables are private.

Other clients take the same command. For Claude Desktop, in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "purser": {
      "command": "python",
      "args": ["/path/to/purser/server/mcp_server.py", "--database", "<your-billing-db>"],
      "env": {"PURSER_TOKEN": "<token of an admin identity>"}
    }
  }
}
```

## Tools

| Tool | What it returns | Backed by |
|---|---|---|
| `list_apps` | Apps, their builds (store, account, delivery) and product catalogs | SQL `game_app`, `product` |
| `overview` | Purchases by status, rows the worker is still processing, counters | `admin_overview` |
| `revenue` | USD totals, daily series, by app / product, subscriptions, paywall views; `from`, `to`, `game_key`, `app_ids`, `include_sandbox` | `admin_revenue_stats` (`summary: true`) |
| `find_player` | Players behind a user id, ledger id, transaction / order id or purchase token | `admin_find_player` |
| `player_profile` | One player's builds, spending, entitlements, purchases and delivery errors | `admin_user_profile` |
| `list_purchases` | Ledger rows by build / app, status and time range | `admin_list_purchases` |
| `check_app` | Setup check of a build or of every build of an app (`offline` to skip store calls) | `admin_check_app` |
| `integration_health` | Integrations per build, events queued / retrying, last dropped-event failures | SQL `integration`, `integration_outbox`, `alert_state` |
| `paywall_results` | A placement's A/B results, or the app's placements and tests | `admin_paywall_results`, SQL `placement`, `paywall_test` |
| `recent_logs` | The last lines of the module log, without the procedure-call notices | `GET /v1/database/<db>/logs` |

With `--allow-writes`:

| Tool | What it does | Backed by |
|---|---|---|
| `grant_manual` | Grants a product without a store purchase (support compensation). Delivered by the worker like any purchase | `admin_grant_manual` |
| `revoke_purchase` | Revokes a purchase (the game gets `revoke` with reason `admin`). Not a refund | `admin_revoke_purchase` |
| `requeue_purchase` | Makes the worker retry a purchase now | `admin_requeue_purchase` |

Clients ask before each tool call unless you allow it, and the write tools are marked as such. `check_app` is not marked read-only either: it saves its report and, online, calls the stores, the delivery target and integration endpoints. Keep writes off for a server you only use for questions.

A failed call (bad arguments, an unknown app, the server unreachable) comes back to the model as a tool error with the module's message; the server keeps running.

## Privacy

Whatever a tool returns goes to the model provider of your MCP client, like any other text in the conversation: player ids, transaction ids, purchase tokens, countries, attribution ids (GAID / IDFV / AppsFlyer id) and revenue. Check that this fits your privacy policy and your agreement with the provider before using it on a production database. Secrets are never returned: the server does not read the `credential` table, and the module never returns secret values from any call.

## Try it without a client

The server reads JSON-RPC messages from stdin, one per line:

```bash
cd server
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"revenue","arguments":{"from":"2026-10-01"}}}' \
  | python mcp_server.py --database <your-billing-db>
```
