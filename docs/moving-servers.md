# Moving to another server

Moving from maincloud to your own server (or between any two SpacetimeDB servers) has two parts. The **data** moves with
`migrate.py`: purchases, settings, keys, admins and server keys, so the new server answers `/entitlements` at once. The
**address** is the hard part: game builds already in players' hands, every store's notification settings and a few
identities know the old one. Plan for the address before the first release, and a move never needs a new game build.

## A stable address of your own

Give clients and stores an address you control, like `https://billing.example.com`, and put a small proxy behind it
that forwards to the database's route base. Moving is then changing where the proxy points: nothing in the games or
the store consoles changes.

```mermaid
flowchart LR
    games["Games<br/>(Unity, SDKs)"] --> proxy["billing.example.com<br/>your proxy"]
    stores["Store notifications<br/>RTDN, App Store, Stripe, ..."] --> proxy
    proxy -->|"today"| old["maincloud<br/>/v1/database/&lt;db&gt;/route"]
    proxy -.->|"after a move"| new["your server<br/>/v1/database/&lt;db&gt;/route"]
```

1. **The proxy** maps `https://billing.example.com/<path>` to `https://<host>/v1/database/<your-billing-db>/route/<path>`
   and passes the method, the body and every header unchanged: store notifications are checked by signatures over the
   raw body (App Store, Amazon, Stripe, Xsolla) or by headers (Telegram's secret token, player tokens). It must not
   cache or rewrite bodies. Caddy in front of maincloud:

    ```caddy
    billing.example.com {
        request_body {
            max_size 1MB
        }
        rewrite * /v1/database/<your-billing-db>/route{uri}
        reverse_proxy https://maincloud.spacetimedb.com {
            header_up Host maincloud.spacetimedb.com
        }
    }
    ```

    On your own server the same block ends with `reverse_proxy localhost:3000` instead (see [self-hosting](self-hosting.md));
    the proxy then exposes only the routes of one database, a narrower public surface than the allowlist. A Cloudflare
    Worker does the same:

    ```js
    export default {
      fetch(request) {
        const url = new URL(request.url)
        url.hostname = "maincloud.spacetimedb.com"
        url.pathname = "/v1/database/<your-billing-db>/route" + url.pathname
        return fetch(new Request(url, request))
      },
    }
    ```

2. **The clients** use it as their server address: `serverBaseUrl` of the Unity config, `baseUrl` of the TypeScript
   SDK, `base_url` of the Godot and Defold SDKs: `https://billing.example.com`.
3. **The server** knows it: Settings → Server → **Public address** = `https://billing.example.com`. The setup check,
   the store setup buttons and the Add an app wizard then show every notification URL with it, so the store consoles
   get the stable address too.
4. **The admin page** stays at the server's own address (`https://<host>/v1/database/<db>/route/admin`): it also calls
   the database's `/call` and `/sql`, which the proxy does not forward.

Check it before a release: `https://billing.example.com/healthz` answers like the server's own `/healthz`, and the
setup check of every build shows notification URLs on `billing.example.com`.

## Everything that knows the server's address

Without a stable address each of these has to be changed when the server moves; with one, only the last four.

| What | Where it is set | Note |
|---|---|---|
| Game clients | `serverBaseUrl` (Unity), `baseUrl` / `base_url` (SDKs) | Builds already published keep the old address until players update. |
| Google Play notifications | Pub/Sub push subscription of every build (its endpoint has the build's `secret`) | `setup_google.py` / Setup tab → Set up Google Cloud updates them. |
| App Store notifications | App Store Connect → the app → App Store Server Notifications, production and sandbox | Setup tab → Set up (Team key) sets both. |
| Amazon Appstore | Developer Console → Real-Time Notifications endpoint | The SNS subscription is confirmed again by the new server. |
| Huawei, Samsung | AppGallery Connect server notification URL; Seller Portal ISN URL of each item | |
| Stripe, Xsolla | Stripe webhook endpoint of every build; Xsolla project webhooks | Setup tab → Set up creates or repairs the Stripe endpoint. |
| Telegram Stars | The bot's webhook | Setup tab → Set the bot webhook. |
| Public address | Settings → Server → Public address | The setup check builds every URL above from it. |
| Admin sign-in | Redirect URI of the SpacetimeAuth (or other provider's) client | The admin page's own address: add the new one. |
| `spacetimedb_idc` delivery | The game database's allowlist of the billing identity | A new database has a new identity, and inter-database calls work only on the same server: a game that stays on the old server switches to `spacetimedb` delivery. |
| Admins with tokens | Tokens made by the old server ("Generate a token") | Another server may not accept them: give those admins new tokens, or invite them by email (sign-in through a provider works on any server). |
| The owner | The identity that published the new database | `migrate.py` keeps the new owner an admin. |

Store keys, the delivery `spacetime_token`, webhook secrets, player token and response signing keys and server keys are
copied with the database: games and backends keep working with them.

## Moving the database

1. Publish the same Purser version on the new server, and allow its delivery hosts (Settings → Server).
2. Copy the database, as the owner of both (every copied table of the new database is replaced):

    ```bash
    python migrate.py --from maincloud --from-db <your-billing-db> --to myserver --to-db new-billing --dry-run
    python migrate.py --from maincloud --from-db <your-billing-db> --to myserver --to-db new-billing
    ```

3. Point the stable address at the new server, or, without one, change everything in the table above right away:
   purchases the old server records after the copy stay there (copy again before the switch to start over).
4. Keep the old server until nothing reaches it any more, then delete it.

Unfinished rows (`next_attempt_at_micros > 0`) are completed by the new server's worker, but while the old server is
still running it may complete them too. That is safe: games apply each `event_id` once, and a consume that was done
already counts as done.
