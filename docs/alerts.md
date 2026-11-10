# Alerts

The billing server can tell you about problems without you opening the admin page: a message to
**Telegram**, **Slack**, **Discord** or **your own webhook** (several at once). Configure it in the
admin page under **Settings → Alerts** (`#alerts`).

## What is reported

Every rule can be switched off; thresholds are in brackets with their defaults.

| Rule (`rule`) | When | Resolved when |
|---|---|---|
| `delivery_failing` | A purchase step failed **3** times in a row (`delivery_attempts`): grant / revoke delivery to the game database or webhook, or the Google consume / acknowledge after it. The ledger never gives up (it retries with backoff for as long as it takes), so "keeps failing" is the signal. | No purchase of the app is pending with that many failed attempts. |
| `store_auth` | Google Play answered 401/403 or rejected the service account key, the App Store answered 401/403, during `/verify`, a store notification or the worker. | The next successful client verification of the app. |
| `integration_failed` | An event to GA4, AppsFlyer, Adjust, Tenjin, AppMetrica, the webhook integration or Cloud Storage was dropped: a non-retryable error, out of retries, or a missing key. | (one-off events) |
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

The state is in the `alert_state` table (one row per rule and app); the **Open problems** list on the
page shows it, and **Dismiss** forgets a problem without a resolved message.

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

One channel per kind; each can be switched off without losing its settings. Secrets are written
once and never shown again; they are stored in `credential` under the `default` scope.

| Channel | Settings (`alert_channel.settings_json`) | Secret (`default/<name>`) | Call |
|---|---|---|---|
| `telegram` | `chat_id`: numeric chat id (`-100…` for groups and channels) or `@channel_name` | `alert_telegram_bot_token` (from @BotFather) | `POST https://api.telegram.org/bot<token>/sendMessage`, `{chat_id, text, parse_mode: "HTML", disable_web_page_preview: true}` |
| `slack` | — | `alert_slack_webhook_url` (Incoming Webhooks) | `POST <url>`, `{text}` |
| `discord` | — | `alert_discord_webhook_url` | `POST <url>`, `{content, allowed_mentions: {parse: []}}` |
| `webhook` | `url` (https) | `alert_webhook_secret` (optional) | `POST <url>`, JSON below |

Telegram: create a bot with @BotFather, add it to the chat (or write to it), then open
`https://api.telegram.org/bot<token>/getUpdates` to find the chat id.

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

`state` is `alert`, `resolved` or `test`. **Send test** on the page sends a `test` message to one
channel right away and shows the answer.

## Delivery

Reducers cannot make HTTP calls, so alerts are queued: the transaction that observes a problem
appends rows to `alert_outbox` (one per enabled channel), and the scheduled `send_alert` procedure
sends them. Network errors, 408, 429 and 5xx are retried (1, 5, 15, 60 minutes; 5 attempts); other
errors are final. The result per channel is stored in `alert_log` (the last 500 messages) and shown
under **Recent alerts**. Bot tokens and webhook URLs are removed from stored error texts.

## Admin calls

| Call | |
|---|---|
| `admin_upsert_alert_channel(json)` | `{"kind": "telegram", "enabled": true, "settings": {"chat_id": "-1001234567890"}, "secret": "<bot token>"}`; an empty `secret` keeps the stored one |
| `admin_delete_alert_channel(kind)` | removes the channel and its secret |
| `admin_set_alert_rules(json)` | `{"delivery_failing": true, "delivery_attempts": 3, "store_auth": true, "integration_failed": true, "notifications_silent": true, "silent_days": 3, "notification_rejected": true, "fx_failing": true, "fx_failing_hours": 24, "window_hours": 6, "send_resolved": true}`; missing fields take these defaults. Stored in `server_state` (`alert_rules`) |
| `admin_test_alert(kind)` (procedure) | sends a test message now; returns `{"ok": true}` or `{"error": "..."}` |
| `admin_dismiss_alert(key)` | forgets an open problem (`alert_state.key` = `{rule}/{app_id}`) |

With the CLI:

```bash
spacetime call <your-billing-db> admin_upsert_alert_channel '{"kind":"slack","secret":"https://hooks.slack.com/services/..."}'
spacetime call <your-billing-db> admin_set_alert_rules '{"window_hours":12,"fx_failing":false}'
```
