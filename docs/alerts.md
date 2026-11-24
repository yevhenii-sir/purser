# Alerts

The billing server can tell you about problems without you opening the admin page: a message to
**Telegram**, **Slack**, **Discord** or **your own webhook**, to as many channels as you like, each
limited to some apps or problems if needed. Configure it in the admin page under **Settings → Alerts**
(`#alerts`).

## What is reported

Every rule can be switched off; thresholds are in brackets with their defaults.

| Rule (`rule`) | When | Resolved when |
|---|---|---|
| `delivery_failing` | A purchase step failed **3** times in a row (`delivery_attempts`): grant / revoke delivery to the game database or webhook, or the Google consume / acknowledge after it. The ledger never gives up (it retries with backoff for as long as it takes), so "keeps failing" is the signal. | No purchase of the app is pending with that many failed attempts. |
| `store_auth` | Google Play answered 401/403 or rejected the service account key, the App Store answered 401/403, during `/verify`, a store notification or the worker. One alert per developer account (see below). | The next successful client verification of the build that last saw it. |
| `integration_failed` | An event to GA4, AppsFlyer, Adjust, Tenjin, AppMetrica, Amplitude, Mixpanel, Meta, the webhook integration or Cloud Storage was dropped: a non-retryable error, out of retries, or a missing key. | (one-off events) |
| `notifications_silent` | The app had production store purchases in the last **3** days (`silent_days`) but no Google RTDN / App Store notification for that long (or never). Checked by the worker from the revenue aggregates. | A notification arrives. |
| `notification_rejected` | A store notification came with a wrong or missing secret in its URL (`google_rtdn_secret`, `apple_notification_secret`). | A notification is accepted. |
| `fx_failing` | The ECB exchange rates could not be refreshed for **24** hours (`fx_failing_hours`). Server-wide. | A refresh succeeds. |

`notifications_silent` assumes that the store notifies about every sale. Google Play sends RTDN for
one-time products only when the real-time developer notifications in Play Console are set to
include them; turn the rule off for apps that sell only one-time products with
subscription-only notifications.

## Throttling

At most **one message per rule and app every 6 hours** (`window_hours`, 1–168). Occurrences inside
the window are counted, and the next message says "Happened N more times since the last message".
When repeats wait a whole window without a new occurrence, the worker sends that summary on its own.
A lasting condition (a silent app, failing rates) is reported when it starts, not every minute.
With `send_resolved` (on by default) a `[RESOLVED]` message follows when the problem is gone.

Store keys belong to developer accounts, so a rejected key is one problem per account, not one per
build: `alert_state.key` is `store_auth/@{account}` (`@default` for builds without an account),
`app_id` is the build that last saw it, and the message says how many builds use the account.

Each channel also has a **flood cap**: at most **10 messages per 10 minutes**. Alerts past the cap are
not sent one by one; the log shows them as `held back (flood cap)`, and when the 10 minutes end the
channel gets one `alerts_held` message that sums them up (rule and build with a count each).

The state is in the `alert_state` table (one row per rule and app, or per rule and account); the
**Open problems** list on the page shows it, and **Dismiss** forgets a problem without a resolved
message.

## Messages

```
[ALERT] Purchase delivery keeps failing: com.example.mygame
google:GPA.1234-5678-9012-34567 (gems, user player-1): grant delivery to the game failed 3 times in a row. Last error: HTTP 503 ...
Happened 2 more times since the last message.
Hint: Check that the game database or webhook is up and accepts the spacetime_token / webhook_secret. ...
Server: <your-billing-db>
https://stdb.example.com/v1/database/<your-billing-db>/route/admin#purchases
```

The server name and the admin link appear when the public address is set (**Server → Public
address**, setting `public_base_url`).

## Channels

A channel (`alert_channel` row) has a unique `name` (defaults to the kind), a `kind`, settings, a
secret and optional filters; it can be switched off without losing them. Add several channels of a
kind to send different apps or problems to different chats. The kind of a channel cannot change. Secrets
are written once and never shown again; they are stored in `credential` under the channel's own
scope `alert-<id>` (`alert-3/alert_telegram_bot_token`), and removing the channel removes its secret.

| Kind | Settings (`alert_channel.settings_json`) | Secret (`alert-<id>/<name>`) | Call |
|---|---|---|---|
| `telegram` | `chat_id`: numeric chat id (`-100…` for groups and channels) or `@channel_name` | `alert_telegram_bot_token` (from @BotFather) | `POST https://api.telegram.org/bot<token>/sendMessage`, `{chat_id, text, parse_mode: "HTML", disable_web_page_preview: true}` |
| `slack` | — | `alert_slack_webhook_url` (Incoming Webhooks) | `POST <url>`, `{text}` |
| `discord` | — | `alert_discord_webhook_url` | `POST <url>`, `{content, allowed_mentions: {parse: []}}` |
| `webhook` | `url` (https) | `alert_webhook_secret` (optional) | `POST <url>`, JSON below |

Telegram: create a bot with @BotFather, add it to the chat (or write to it), then open
`https://api.telegram.org/bot<token>/getUpdates` to find the chat id.

### Routing

Every alert goes to each enabled channel whose filters match:

- **Apps** (`game_keys`, app keys): only alerts about builds of those apps. Server-wide alerts
  (`fx_failing`) have no app, so they go only to channels **without** an app filter.
- **Rules** (`rules`, rule ids from the table above): only those problems.

Empty filters mean every app (and the server-wide alerts) and every rule. Example: one Telegram
channel for the whole team without filters, plus a Slack channel per game studio with its apps and
only `delivery_failing` and `store_auth`.

Webhook body (`X-Billing-Event: alert`; with a signing key also
`X-Billing-Signature: hex(HMAC-SHA256(key, body))`, like the other webhooks):

```json
{
  "type": "alert",
  "rule": "store_auth",
  "state": "alert",
  "app_id": "com.example.mygame",
  "title": "[ALERT] Store rejected the credentials: com.example.mygame",
  "detail": "Google purchase lookup: HTTP 401: ...",
  "hint": "Check the Google service account ...",
  "repeats": 0,
  "link": "https://stdb.example.com/v1/database/<your-billing-db>/route/admin#app/mygame/setup",
  "server": "<your-billing-db>",
  "at_ms": 1790000000000,
  "text": "the whole message as plain text"
}
```

`state` is `alert`, `resolved` or `test`. **Test** on the page sends a `test` message to one
channel right away (even a disabled one) and shows the answer.

## Delivery

Reducers cannot make HTTP calls, so alerts are queued: the transaction that observes a problem
appends rows to `alert_outbox` (one per matching channel, with `channel_id` and the channel's name),
and the scheduled `send_alert` procedure sends them. Network errors, 408, 429 and 5xx are retried (1,
5, 15, 60 minutes; 5 attempts); other errors are final. The result per channel is stored in
`alert_log.results_json` (keyed by channel name; the last 500 messages) and shown under **Recent alerts**. Bot tokens and webhook URLs are removed from stored error texts.

## Admin calls

| Call | |
|---|---|
| `admin_upsert_alert_channel(json)` | `{"id": 0, "name": "ops", "kind": "telegram", "enabled": true, "settings": {"chat_id": "-1001234567890"}, "secret": "<bot token>", "game_keys": ["mygame"], "rules": ["delivery_failing", "store_auth"]}`. `id` 0 or missing: a new channel, or the one with that `name`; `name` defaults to the kind; `enabled` defaults to true; an empty `secret` keeps the stored one; empty `game_keys` / `rules` = all. The kind of an existing channel cannot change. Errors: `[ERR_BAD_ALERT]` (unknown kind, rule or app, bad settings, a name taken by another channel), `[ERR_ALERT_SECRET_MISSING]` (an enabled channel without its secret; the webhook's is optional) |
| `admin_delete_alert_channel(id)` | removes the channel and its secret |
| `admin_set_alert_rules(json)` | `{"delivery_failing": true, "delivery_attempts": 3, "store_auth": true, "integration_failed": true, "notifications_silent": true, "silent_days": 3, "notification_rejected": true, "fx_failing": true, "fx_failing_hours": 24, "window_hours": 6, "send_resolved": true}`; missing fields take these defaults. Stored in `server_state` (`alert_rules`) |
| `admin_test_alert(id)` (procedure) | sends a test message to the channel now, even when it is disabled; returns `{"ok": true}` or `{"error": "..."}` |
| `admin_dismiss_alert(key)` | forgets an open problem (`alert_state.key` = `{rule}/{app_id}`, or `store_auth/@{account}`) |

With the CLI:

```bash
spacetime call <your-billing-db> admin_upsert_alert_channel '{"kind":"slack","secret":"https://hooks.slack.com/services/..."}'
spacetime call <your-billing-db> admin_upsert_alert_channel '{"name":"mygame-slack","kind":"slack","secret":"https://hooks.slack.com/services/...","game_keys":["mygame"],"rules":["delivery_failing"]}'
spacetime call <your-billing-db> admin_test_alert 1
spacetime call <your-billing-db> admin_set_alert_rules '{"window_hours":12,"fx_failing":false}'
```
