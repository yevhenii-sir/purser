# Troubleshooting

Errors carry a code: `[ERR_…] message` in admin calls (the admin page shows the message), `"code": "ERR_…"` in the JSON
of client and server routes. The setup check (app → Check, or `python seed.py --check`) explains most configuration
problems before they reach players; the Logs page shows what the module did.

## Purchases and players

| Code | Where | What it means | What to do |
|---|---|---|---|
| `ERR_CLIENT_AUTH` | client routes, `401` | The build has `client_auth = signed` and the request has no player token, an expired one, or one for another `user_id` / app. | Have your backend sign a fresh HS256 token for this player ([game-integration.md](game-integration.md#client-requests-player-tokens-limits-signed-answers)). |
| `ERR_RATE_LIMITED` | client and server routes, `429` | Too many requests of this player or build (client routes) or key (`/server/*`). | Wait `Retry-After` seconds; the Unity package does it by itself. A burst from one player id is usually a client loop. |
| `ERR_TX_CLAIMED` | `/verify` | The store transaction belongs to another player id (consumables never move). | Normal for a replayed receipt; if the player id changed (reinstall), use stable ids. |
| `ERR_CONSUMED_ELSEWHERE` | `/verify` (Google) | A consumable is consumed in Google Play but this ledger never had it: granted by another system or a replay. | When another billing system or the client still consumes, turn on *Client consumes purchases itself* on the build. |
| `ERR_TRANSFER_LIMIT` | `/restore` | Restoring a purchase on another player id is not allowed by the app's transfer policy now. | Delivery tab → Transfer policy; support can move it by hand (player page → Move). |
| `ERR_NOT_TRANSFERABLE` | admin move | Consumables stay with the player who bought them. | — |
| `ERR_PERSONAL_ATTRIBUTE` | `/attributes`, `403` | `email`, `phone`, `name` … are accepted only from builds with player tokens. | Use player tokens, or keep personal data out of attributes. |
| `ERR_PURCHASE_NOT_FOUND` | admin, `/server/revoke` | No such ledger id (or not one of the key's app). | Ledger ids are `{store}:{transaction_id}`. |
| `ERR_NOT_REVOCABLE` | admin, `/server/revoke` | The purchase cannot be revoked in its state (still being verified, already ended). | Look at its history on the purchase page. |
| `ERR_TX_EXISTS` | manual grant, `/server/grant` (`409`) | The `transaction_id` of a grant is already used (by another grant). | Use one id per grant; repeat the same request to get its result. |
| `ERR_BAD_USER` | everywhere | A player id must be 1-128 characters of `A-Z a-z 0-9 - _ . :`. | — |
| `ERR_PLAYER_TOKENS_REQUIRED` | `/manage/stripe`, `403` | The customer portal acts for the player's Stripe customer: only builds with player tokens get a session. | Use player tokens, or Stripe's customer portal login link (`StripeCheckoutOptions.CustomerPortalLoginUrl`). |
| `ERR_NO_SUBSCRIPTION` | `/manage/stripe`, `404` | The player has no active Stripe subscription in this build. | The login link still lets a customer in by email. |
| `ERR_OFFER_KEY` | `/offers/apple/sign` | The App Store In-App Purchase key for promotional offers is not configured. | Keys tab ([apple-offers.md](apple-offers.md)). |

A purchase that stays *pending* on the Overview has a `last_error`: a store or delivery call that keeps failing
(credentials, the game's reducer or webhook). The worker retries with backoff; fix the cause and it goes on by itself,
or use **Retry now**. `GET /healthz` reports `pending_purchases`, `due_purchases`, `oldest_due_secs` and
`stuck_purchases` for monitors.

## Server keys (`/server/*`)

| Code | What it means |
|---|---|
| `ERR_SERVER_KEY` (`401`) | No `Authorization: Bearer psk_…`, a malformed key, or an unknown / revoked one. |
| `ERR_SERVER_SCOPE` (`403`) | The key lacks the scope (`read`, `grant`, `revoke`), or the build is not one of the key's app. |
| `ERR_BAD_GRANT` (`400`) | The grant is invalid: unknown product of the build, invalid player id. |
| `ERR_BAD_KEY`, `ERR_KEY_EXISTS`, `ERR_KEY_NOT_FOUND` | Making or revoking a key on the admin page: make a new key and try again. |

## Admin page and admin calls

| Code | What it means |
|---|---|
| `ERR_ADMIN_UNAUTHORIZED` | The token is not an admin of this database. The owner adds admins on the Admins page. |
| `ERR_ROLE` | Your role cannot do this (viewer, support, editor, admin). |
| `ERR_SCOPE` | You are limited to some apps and this is another app or a server-wide setting. |
| `ERR_OWNER_ONLY` | Only the database owner (the identity that published it) can do this: imports, reindexing, delivery hosts. |
| `ERR_ADMIN_PROTECTED`, `ERR_ADMIN_SELF`, `ERR_ADMIN_SELF_REMOVE` | The owner cannot be changed; nobody changes their own role or removes themselves. |
| `ERR_BAD_JSON` | The input is not the expected JSON (the message names the field). |
| `ERR_BAD_APP`, `ERR_UNKNOWN_APP` | No such build or app, or a build field is invalid. |
| `ERR_BAD_PRODUCT` | A product field is invalid (kind, price, access levels). |
| `ERR_BAD_CREDENTIAL` | Not a known secret name for this scope. |
| `ERR_BAD_INTEGRATION` | Integration settings are invalid (the message says which field). |
| `ERR_BAD_SETTING` | Unknown setting or a value out of range. |
| `ERR_BAD_POLICY` | Transfer policy values out of range. |
| `ERR_BAD_ROLE`, `ERR_BAD_IDENTITY`, `ERR_BAD_NAME` | Admins page input. |
| `ERR_BAD_HOST` | A self-hosted delivery host must be an https host name. |
| `ERR_BAD_FX` | A manual exchange rate needs an ISO currency and a positive rate. |
| `ERR_BAD_ALERT`, `ERR_ALERT_SECRET_MISSING` | Alert channel input; set the channel's secret first. |
| `ERR_BAD_QUERY` | An admin read without the index it needs (purchases are read by id, player, transaction or token). |
| `ERR_BAD_IMPORT` | An imported row is not a valid ledger row, or its build is not configured here. |
| `ERR_STRIPE_WEBHOOK_EXISTS` | A Stripe endpoint for this build exists but its signing secret is not stored here: paste it on the Keys tab or delete the endpoint in Stripe. |

## Paywalls and A/B tests

| Code | What it means |
|---|---|
| `ERR_BAD_PAYWALL`, `ERR_BAD_PLACEMENT`, `ERR_BAD_TEST`, `ERR_BAD_EVENT` | Invalid paywall, placement, test or event input (the message names the field). |
| `ERR_PAYWALL_NOT_FOUND`, `ERR_PLACEMENT_NOT_FOUND`, `ERR_UNKNOWN_TEST`, `ERR_UNKNOWN_VARIANT`, `ERR_NO_TEST` | The paywall, placement, test or variant does not exist (any more). |
| `ERR_PAYWALL_IN_USE` | A placement or a running test still shows the paywall. |
| `ERR_TEST_RUNNING` | A placement has a running test: stop it first. |

## Operations

- **`/healthz` answers `503 worker_stopped`**: the ledger worker has not run for 5 minutes. The Overview offers to
  restart it (`admin_ensure_worker`); after a publish it starts by itself.
- **`/healthz` says `backlog`** (and the `queue_behind` alert fired): a purchase has waited 15 minutes after it became
  due. The errors of the waiting purchases on the Overview say why: usually a store or the game's server that answers
  slowly or not at all (its rows wait a minute after every call without a reply). When the calls succeed but there are
  more purchases than the drainers take, raise `worker_lanes` (`admin_set_setting worker_lanes 4`, at most 8); each
  drainer holds one module instance while it waits ([self-hosting.md](self-hosting.md#capacity)).
- **An integration dropped events** (Integrations tab shows *The last event was dropped*): fix its key or settings,
  then send the dropped events again (`admin_replay_integration_events(app_id, kind)`; they are kept for 30 days).
- **After restoring with SQL** (a backup, an import done by hand): `admin_reindex_ledger` rebuilds the worker's queue
  and the players' owned purchases; `backup.py restore` runs it, and the worker finishes an interrupted one.
- **A restore stopped half-way**: run `backup.py restore … --resume`.
