# Self-hosting

The billing module runs on any SpacetimeDB **2.16.1+** server: maincloud or your own. This page covers your own server.

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

Keep `publish`, `sql`, `logs` and other admin paths closed to the internet and use them through an SSH tunnel. The admin page itself (`/route/admin`) is harmless at the public address (it loads no data without `/sql`); the Docker Caddyfile hides it anyway with `not path_regexp ^/v1/database/[^/]+/route/admin(/.*)?$` in the `@public` matcher.

Caddy:

```caddy
stdb.example.com {
    @public path_regexp ^/v1/(database/[^/]+/(route|call|subscribe|schema)(/.*)?|identity.*|ping)$
    handle @public {
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
}
```

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
- **Public address:** set it in Settings → Server → Public address (`https://stdb.example.com/v1/database/<db>/route`), so the setup check and the Add an app wizard show the right URLs for the stores and for the Unity config while you are on `localhost`.

## Giving a coworker access

Admins → **Add admin** → name, role (Admin, Editor, Support or Viewer) → **Generate a token**. Send the token and the admin page link privately: the token is shown once, and if it is lost you remove that admin and add a new one. Removing an admin stops their token at once.

- The server checks the role on every request. Admin-page actions are recorded in the Audit log (Settings → Audit log).
- A coworker's token reads through `/call` (the `admin_select` procedure), not `/sql`. So their admin page works through the same tunnel as yours, or at the public address if you allow `/v1/database/<db>/route/admin` and `/call` there.
- Logs need the owner.
- Someone with their own SpacetimeDB login can send you their identity (`spacetime login show --identity`) instead; add it in the same dialog.

## Delivery to games on the same server

The module cannot call private addresses (`127.0.0.1`, `10.x`, …). Deliveries to a game on the same machine go through the public hostname, which must be allowed by the database owner: Settings → Server → SpacetimeDB hosts → `stdb.example.com` (or `admin_add_spacetime_host`). The `call` path must be open in the proxy.

## Upgrading

Publish the new `purser.wasm` to the same database name. New tables and indexes are created automatically.

A release whose notes say it needs `--delete-data` changes tables in a way SpacetimeDB cannot migrate in place. Keep the data with a backup around the publish (from `server/`, as the database owner):

```bash
python backup.py backup  --server maincloud --database <your-billing-db> --dir backups/before-upgrade
spacetime publish --delete-data -b purser.wasm <your-billing-db>
python backup.py restore --server maincloud --database <your-billing-db> --dir backups/before-upgrade
```

`backup.py` reads every table through the owner's `/sql` (no caches, queued integration events or worker schedules) and writes it back into the new database. A newer module's added columns get their zero value and removed ones are dropped; when a release needs more than that, its notes say how to carry the data over. Large tables are read in windows, the files are readable by their owner only (they hold every secret), and a restore that stopped goes on with `--resume`.

## Memory

Tables whose rows pile up keep only what is used in memory (SpacetimeDB tiered storage): the purchase log, the admin audit and the daily revenue aggregates may leave memory, purchases and player data stay at least on the server's disk, and settings, queues and worker state stay in memory. Maincloud limits a database to 24 GiB of resident memory; the ledger stays far below it, even with millions of purchases.

## Capacity

Measured on a local SpacetimeDB 2.16.1 (32 threads; `tools/loadtest.py`, the store replaced by a local proxy):

| Load | Answers per second | Latency (median / p99) |
|---|---|---|
| `/entitlements` (reads) | ~7,500 | 2 / 3 ms |
| a purchase without outbound calls (`delivery: none`, `/server/grant`) | ~2,500 | 6 / 8 ms |
| a purchase whose store or game answers in 300 ms | ~105 | 310 ms (more at once: waits in line) |
| a store that does not answer | ~3 s per request, the purchase is retried | |
| a queue of 18,800 due purchases (calls that succeed at once) | emptied ~5 s after the drainers started | |

The limit that matters is the store's latency. SpacetimeDB runs HTTP routes and procedures on a pool of module instances, one per CPU thread by default, and a route that waits for a store keeps its instance for the whole wait: purchases per second ≈ pool size ÷ the store's answer time, and while every instance waits, the other routes (`/entitlements` at a game's start) wait in line behind them. Nothing is lost meanwhile: requests queue, and a purchase the client did not get an answer for stays unfinished in the store and is sent again.

- On your own server, raise the pool in the server's `config.toml` (in its data directory) when the stores are slow: the instances mostly wait for the network, so more than one per thread is fine (2-4 × the threads, memory permitting):

  ```toml
  [wasm]
  procedure-instance-pool-size = 64
  ```

- On maincloud the pool is not configurable.
- A request spends at most 3 s on outbound calls (a store that hangs cannot hold an instance longer), and the queue drainers hold at most `worker_lanes` instances (server setting, default 2, up to 8). With a store answering in 300 ms, 2 drainers finish ~6 queued purchases a second, 8 about 25: raise it when `/healthz` says `backlog` although the calls succeed.

## Backups and monitoring

- **Backups:** `python backup.py backup --dir backups/$(date +%F)` from a scheduled job (cron, a CI schedule) with the owner token in `PURSER_TOKEN`. Keep the files private: they hold the store keys and secrets in `credential`.
- **Monitoring:** watch `GET <route>/healthz` with any uptime monitor. It answers `200 {"status": "healthy"}` while the ledger worker runs, `503 {"status": "worker_stopped"}` when it has not run for 5 minutes (purchases, renewals and alerts wait until it runs again; the admin page's Overview offers to restart it), `starting` right after a publish, and `200 {"status": "backlog"}` when a purchase has waited in the queue for 15 minutes (`due_purchases`, `oldest_due_secs`; see [Capacity](#capacity)). A monitor that alerts on numbers can watch `oldest_due_secs`.
