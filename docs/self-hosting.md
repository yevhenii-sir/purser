# Self-hosting

The billing module runs on any SpacetimeDB **2.16.3+** server: maincloud or your own. This page covers your own server.

**Quickest path: Docker.** [`deploy/docker`](../deploy/docker/README.md) runs SpacetimeDB with `docker compose`, publishes the module with `publish.sh` (the release download or your own build) and adds an optional Caddy proxy with automatic HTTPS that applies the allowlist below. Its README covers the owner token, backups and upgrades. The rest of this page explains the same setup by hand.

## Run SpacetimeDB

Install the SpacetimeDB CLI of the same version as the module (see the release notes) and run the server under a service manager (systemd, Docker, …):

```bash
spacetime start --listen-addr 127.0.0.1:3000 --data-dir /var/lib/spacetimedb
```

The server listens on `localhost:3000` only; a reverse proxy publishes it with TLS.

## Expose only what clients and stores need

Stores, game clients and game servers need these paths:

| Path | Used by |
|---|---|
| `/v1/database/*/route/*` | billing HTTP API: `/verify`, `/restore`, `/entitlements`, store notifications, the admin page |
| `/v1/database/*/call/*` | delivery to games on this server, game clients calling reducers |
| `/v1/database/*/subscribe` | game clients |
| `/v1/identity*`, `/v1/ping` | clients |
| `/v1/database/*/schema` | optional: the setup check reads the game schema to verify the grant / revoke reducers |

Keep `publish`, `sql`, `logs` and other admin paths closed to the internet and use them through an SSH tunnel. The server also answers, without any token: `/v1/metrics` (Prometheus metrics of every database), `/v1/prometheus/sd_config`, `/v1/health`, `/v1/identity/<identity>/databases` (the databases an identity owns) and, outside `/v1`, `/internal/heap` and `/internal/task-dump` (memory profiling, task dumps): the allowlists below keep all of them inside; never expose port 3000 itself. The admin page itself (`/route/admin`) is harmless at the public address (it loads no data without `/sql`); the Docker Caddyfile hides it anyway with `not path_regexp ^/v1/database/[^/]+/route/admin(/.*)?$` in the `@public` matcher.

Caddy:

```caddy
stdb.example.com {
    @public path_regexp ^/v1/(database/[^/]+/(route|call|subscribe|schema)(/.*)?|identity.*|ping)$
    handle @public {
        request_body {
            max_size 1MB
        }
        reverse_proxy localhost:3000
    }
    respond 404
}
```

nginx:

```nginx
server {
    listen 443 ssl http2;
    server_name stdb.example.com;
    # ssl_certificate ...;

    location ~ ^/v1/(database/[^/]+/(route|call|subscribe|schema)(/.*)?|identity.*|ping)$ {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
    }
    location / { return 404; }
    # The server buffers request bodies whole and has no limit or timeout of its own on routes.
    client_max_body_size 1m;
    proxy_read_timeout 60s;
}
```

More that SpacetimeDB does not do for you (from its source; none of it is configurable in the server):

- **Request size and rate:** `/route/*` reads any body whole into memory, and nothing limits requests per client: set the proxy's body limit (Caddy `request_body { max_size 1MB }`) and, if the server is exposed widely, a rate limit. Billing limits per player and per build itself, but only after the request reached the module.
- **`X-Forwarded-Host` / `-Proto`** are taken from any client: the proxy should set them (`proxy_set_header Host $host;` above) rather than pass the client's.
- **Disk:** module logs rotate daily and are never deleted (`<data-dir>/replicas/*/module_logs`), the server log is never rotated, and old snapshots and commitlog segments stay. Delete old files with a cron job or a logrotate rule; watch the free space.
- **`config.toml`** (in the data directory) ignores unknown keys and typos silently, and the file `spacetime start` writes the first time logs at `debug`: set `[logs] level = "info"` and drop the debug `directives` on a production server.
- **The Postgres wire protocol** (`--pg-port`) has no TLS on your own server and logs a token that failed to sign in: leave it off, or on `127.0.0.1` only.
- **Production databases:** `spacetime lock <your-billing-db>` refuses `--delete-data`, `spacetime delete` and resets until `spacetime unlock`. Lock the production database: `spacetime dev` publishes with `--delete-data=on-conflict` by default, and to maincloud when no server is set.

## Admin access through an SSH tunnel

```bash
ssh -N -L 3001:localhost:3000 user@stdb.example.com
spacetime server add myserver --url http://localhost:3001
```

Then, from `server/` in a second terminal:

```bash
spacetime publish -s myserver --bin-path purser.wasm <db>
python seed.py --server myserver --database <db> --check
spacetime logs -s myserver <db>
```

- **Admin panel:** `http://localhost:3001/v1/database/<db>/route/admin`. The page calls `/sql`, `/logs` and `/call` of its own database, so it works fully through the tunnel. At the public address it opens, but data does not load (`/sql` is closed).
- **Public address:** set it in Settings → Server → Public address (`https://stdb.example.com/v1/database/<db>/route`, or a stable address of your own in front of it: [moving to another server](moving-servers.md#a-stable-address-of-your-own)), so the setup check and the Add an app wizard show the right URLs for the stores and for the Unity config while you are on `localhost`.

## Giving a coworker access

Admins → **Add admin** → name, role (Admin, Editor, Support or Viewer) → **Generate a token**. Send the token and the admin page link privately: the token is shown once, and if it is lost you remove that admin and add a new one. Removing an admin stops their token at once.

- The server checks the role on every request. Admin-page actions are recorded in the Audit log (Settings → Audit log).
- A coworker's token reads through `/call` (the `admin_select` procedure), not `/sql`. So their admin page works through the same tunnel as yours, or at the public address if you allow `/v1/database/<db>/route/admin` and `/call` there.
- Logs need the owner.
- Someone with their own SpacetimeDB login can send you their identity (`spacetime login show --identity`) instead; add it in the same dialog.

### Sign-in with Google, GitHub and other accounts

Instead of handing out tokens, admins can sign in through an OpenID Connect provider: [SpacetimeAuth](https://spacetimedb.com/docs/core-concepts/authentication/spacetimeauth/) on maincloud (Google, GitHub, Discord, Twitch, Kick, magic links), or any provider whose tokens your SpacetimeDB server accepts (Keycloak, Auth0, Zitadel, ...) on your own server. Tokens keep working: the owner still signs in with the CLI token, and the token sign-in stays as a fallback.

1. **The provider.** On maincloud: the database's dashboard → **SpacetimeAuth** → *Use SpacetimeAuth*, choose the identity providers, and in **Clients** take the client ID (not a secret). Elsewhere: a public client (no secret) with the authorization code flow and PKCE.
2. **The redirect URI.** Register the admin page's address exactly as the browser opens it, without `#…`: `https://maincloud.spacetimedb.com/v1/database/<your-billing-db>/route/admin` (or your server's address of it).
3. **Purser.** Admins → **Sign-in with a provider** → *Set up*: the issuer (`https://auth.spacetimedb.com/oidc` for SpacetimeAuth) and the client ID. Purser reads the provider's discovery document right away and refuses an issuer it does not name exactly or a provider without PKCE (`S256`). The page then shows **Sign in with …** and its Content-Security-Policy lets it call the provider's token endpoint, nothing else.
4. **Invite.** Admins → **Add admin** → name, role, apps → the coworker's email → **Invite**. They open the admin page, sign in with the provider using that email, and become an admin with that role. An invitation works once, for 7 days, and only with an email the provider marks as verified.

#### Domain rules

A domain rule lets everyone with a verified email of a company domain in without an invitation: Admins → **Domain rules** → *Add a rule* → the domain (`studio.com`), the role (**Viewer** or **Support** only), the apps. Their first sign-in through the provider makes them an admin with that role; the list shows them as *via @studio.com*.

- **Organization check.** With *Only accounts of the organization* on, the token must also carry `hd` equal to the domain: Google Workspace sets it for accounts of the organization, so a personal Google account with a company address is refused. Use it with Google as the provider (an OpenID Connect client in Google Cloud). SpacetimeAuth does not pass `hd` today: with the check on, nobody gets in through it.
- **Every request.** An admin of a rule acts only while the rule exists and every token still shows a verified address of the domain (and `hd`, when required). Removing the rule removes its admins at once.
- **Removing one person.** Removing an admin who joined through a rule keeps their email out of the rule; only an invitation lets them back in.
- **Changing one person.** Changing the role or apps of such an admin by hand detaches them from the rule: from then on they are like an invited admin, and the rule's changes or removal no longer touch them.

**Rules of use.** A domain rule trusts the provider's word that an address belongs to someone who works there. Follow these, or invite people one by one:

1. Use it only with a provider the company controls (Google Workspace, the company's Keycloak, Okta, Entra ID), where an account ends when the person leaves the company. Turn the organization check on where the provider supports it.
2. Never with providers of personal accounts (GitHub, Discord, Twitch): an address verified there stays verified on the person's own account after they leave the company. Public email domains (gmail.com, outlook.com, ukr.net, …) are refused.
3. Give the lowest role that does the job: Viewer reads revenue, purchases and settings, Support also grants, revokes and refunds purchases. Admins and editors are invited one by one; a rule cannot give their roles.
4. Limit the rule to the apps the team works on when it does not need every app.
5. Look through the admin list after people leave or join: everyone of a rule shows *via @domain*, and the Audit log records every join (`admin_accept_invite`) and every change of a rule.
6. Remove the rule when it is no longer needed, or when the company changes its email domain or provider.

Admins who joined by invitation are bound to the provider and client they signed in with: SpacetimeDB checks a token's signature and expiry but not which application it was issued for, so Purser refuses their identity with a token of another client of the same provider. Removing them on the Admins page ends their access at once. Provider tokens are short-lived: when one expires, the page asks to sign in again (usually one click while the provider's session lasts).

## Delivery to games on the same server

A game database on the same SpacetimeDB server is best reached with the `spacetimedb_idc` delivery: inter-database calls inside the server, no HTTP, no token, nothing to open in the proxy ([game integration](game-integration.md)). The build's `spacetime_db` is the game database's identity.

The `spacetimedb` delivery (HTTP) cannot call private addresses (`127.0.0.1`, `10.x`, …). Deliveries to a game on the same machine then go through the public hostname, which must be allowed by the database owner: Settings → Server → SpacetimeDB hosts → `stdb.example.com` (or `admin_add_spacetime_host`). The `call` path must be open in the proxy.

## Upgrading

Publish the new `purser.wasm` to the same database name. New tables and indexes are created automatically. To know beforehand whether a module publishes in place, ask the server (nothing is published; from `server/`, as the database owner):

```bash
python publish_check.py --server maincloud --database <your-billing-db> --wasm purser.wasm
```

It prints SpacetimeDB's migration plan and exits with 0 (publish normally) or 2 (the release needs `--delete-data`: back up and restore as below). A locked database (`spacetime lock`) refuses `--delete-data` until you unlock it.

A release whose notes say it needs `--delete-data` changes tables in a way SpacetimeDB cannot migrate in place. Keep the data with a backup around the publish (from `server/`, as the database owner):

```bash
python backup.py backup  --server maincloud --database <your-billing-db> --dir backups/before-upgrade
spacetime publish --delete-data -b purser.wasm <your-billing-db>
python backup.py restore --server maincloud --database <your-billing-db> --dir backups/before-upgrade
```

`backup.py` reads every table through the owner's `/sql` (no caches, queued integration events, inter-database calls in flight or worker schedules) and writes it back into the new database. A newer module's added columns get their zero value (or the value older rows had, `NEW_COLUMN_VALUES` in `backup.py`) and removed ones are dropped; when a release needs more than that, its notes say how to carry the data over. Large tables are read in pages the module reads through an index (`admin_backup_page`: `/sql` would scan a whole table for every window, holding the database meanwhile), the files are readable by their owner only (they hold every secret), and a restore that stopped goes on with `--resume`.

## Memory

Tables whose rows pile up keep only what is used in memory (SpacetimeDB tiered storage): the purchase log, the admin audit and the daily revenue aggregates may leave memory, purchases and player data stay at least on the server's disk, and settings, queues and worker state stay in memory. Maincloud limits a database to 24 GiB of resident memory; the ledger stays far below it, even with millions of purchases.

## Capacity

Measured on a local SpacetimeDB 2.16.1 with the data on disk (Windows, NVMe SSD, 32 threads; `tools/loadtest.py`, the store replaced by a local proxy). Every transaction a request opens waits until it is on disk (fsync); concurrent ones share a flush, so throughput grows with concurrency while one request alone waits a few milliseconds per transaction:

| Load | Concurrent requests | Answers per second | Latency (median / p99) |
|---|---|---|---|
| `/entitlements` (reads) | 1 / 16 / 64 | ~280 / ~2,900 / ~4,700 | 3 / 6, 5 / 36, 10 / 43 ms |
| a purchase without outbound calls (`delivery: none`, `/server/grant`) | 1 / 16 / 64 | ~67 / ~800 / ~1,400 | 13 / 47, 19 / 52, 43 / 78 ms |
| a purchase whose store or game answers in 300 ms | 64 | ~105 | 650 ms (waits in line for an instance) |
| `/entitlements` while those purchases fill the instances | 4 | ~11 | 340 / 630 ms |
| a store that does not answer | 8 | the request answers after 3 s, the purchase is retried | |
| a queue of 3,700 due purchases (calls that succeed at once) | | emptied ~12 s after the drainers started | |

In memory (`spacetime start --in-memory`, no disk) the same machine answered ~7,500 reads and ~2,500 purchases a second at 16 concurrent requests (with SpacetimeDB 2.16.3 and the 0.11 module: ~5,800 reads at 16 and at 64, where the per-build rate limit starts answering 429, and ~2,000 purchases at 16; purchases whose game answers in 300 ms: ~110 a second at 64): on your server, the disk sets the pace of everything that writes, and every request writes (rate limits, the ledger). A drainer starts at once for work a request leaves; a retry that becomes due later waits for the minute worker to start one.

The limit that matters is the store's latency. SpacetimeDB runs HTTP routes and procedures on a pool of module instances, one per CPU thread by default, and a route that waits for a store keeps its instance for the whole wait: purchases per second ≈ pool size ÷ the store's answer time, and while every instance waits, the other routes (`/entitlements` at a game's start) wait in line behind them. Nothing is lost meanwhile: requests queue, and a purchase the client did not get an answer for stays unfinished in the store and is sent again.

- On your own server, raise the pool in the server's `config.toml` (in its data directory) when the stores are slow: the instances mostly wait for the network, so more than one per thread is fine (2-4 × the threads, memory permitting):

  ```toml
  [wasm]
  procedure-instance-pool-size = 64
  ```

- On maincloud the pool is not configurable.
- The pool helps with waiting, not with computing: a database computes one call at a time. A server-wide revenue report over a long range counted in one call would keep everything else waiting (2,000 builds, 90 days: ~13 s); the full report is counted in the background a batch per call instead, and kept for an hour; `analytics` = `light` or `off` does not count it at all (see [Analytics](analytics.md#analytics-modes)). The purchase worker never reads a purchase's history: what it needs is kept on the row.
- A request spends at most 3 s on outbound calls (a store that hangs cannot hold an instance longer), and the queue drainers hold at most `worker_lanes` instances (server setting, default 2, up to 8). With a store answering in 300 ms, 2 drainers finish ~6 queued purchases a second, 8 about 25: raise it when `/healthz` says `backlog` although the calls succeed.

## Large games

What grows with a game is what Purser keeps, not the purchases per second: a purchase costs under a millisecond of
the database's time, so even a few hundred a second are far from the limit, while a year of a large game is tens of
millions of ledger rows and millions of players who own something. For games of that size:

- **One Purser database per game** (or per game and region), each with its own builds: a database computes one call
  at a time, so separate games should not wait for each other. They can share a SpacetimeDB server; give each its own
  admin page link and secrets.
- **`analytics` = `off`** (Server → Analytics): the database keeps only what purchases need (the ledger, what players
  own, delivery, store notifications, integrations). Nothing walks over all subscriptions every day and no report
  reads the ledger. Revenue goes to your attribution and analytics tools as it happens (every purchase, renewal and
  refund is an [integration](../server/README.md#integrations-ga4-appsflyer-adjust-tenjin-appmetrica-amplitude-mixpanel-meta-webhook-cloud-storage)
  event: Tenjin, AppsFlyer, Adjust, GA4, ...) and to BigQuery through the Cloud Storage export
  ([docs/bigquery.md](bigquery.md)), where cohorts and LTV over years of purchases are counted without holding the
  database that takes payments. Business alerts (purchases that stopped, refund spikes, revenue drops) and `/metrics`
  keep working: they read each build's daily sales, counted in every mode.
- `full` analytics count the full report in the background a batch at a time, so even a large ledger does not hold
  purchases while it is counted; but the daily subscription snapshot and the last step of the report read every
  subscription players own in one call (~1 s per ~25,000 owned items in a wasm module), which is what `off` avoids.

`cargo test -p purser --release profile -- --ignored --nocapture` (`PROFILE_SIZES=1000,100000`) times every feature of
the module at growing ledger sizes on your machine.

## Backups and monitoring

- **Backups:** `python backup.py backup --database <your-billing-db> --dir backups/$(date +%F)` from a scheduled job (cron, a CI schedule) with the owner token in `PURSER_TOKEN`. Keep the files private: they hold the store keys and secrets in `credential`. On Linux and macOS the script writes them readable by you only; on Windows they get the folder's permissions, so keep the folder private (or on an encrypted drive).
- **Monitoring:** watch `GET <route>/healthz` with any uptime monitor. It answers `200 {"status": "healthy"}` while the ledger worker runs, `503 {"status": "worker_stopped"}` when it has not run for 5 minutes (purchases, renewals and alerts wait until it runs again; the admin page's Overview offers to restart it), `starting` right after a publish, and `200 {"status": "backlog"}` when a purchase has waited in the queue for 15 minutes (`due_purchases`, `oldest_due_secs`; see [Capacity](#capacity)). Prometheus can scrape `GET <route>/metrics` (create its token on the admin page's Server page; [server/README.md](../server/README.md#get-metrics)). A monitor that alerts on numbers can watch `oldest_due_secs` and `blocked_purchases` (purchases that wait for a fix of the setup: [troubleshooting](troubleshooting.md#purchases-and-players)).
