# Self-hosting

The billing module runs on any SpacetimeDB **2.14.1+** server: maincloud or your own. This page covers your own server.

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
- A coworker's token reads through `/call` (the `admin_read` procedure), not `/sql`. So their admin page works through the same tunnel as yours, or at the public address if you allow `/v1/database/<db>/route/admin` and `/call` there.
- Logs need the owner.
- Someone with their own SpacetimeDB login can send you their identity (`spacetime login show --identity`) instead; add it in the same dialog.

## Delivery to games on the same server

The module cannot call private addresses (`127.0.0.1`, `10.x`, …). Deliveries to a game on the same machine go through the public hostname, which must be allowed by the database owner: Settings → Server → SpacetimeDB hosts → `stdb.example.com` (or `admin_add_spacetime_host`). The `call` path must be open in the proxy.

## Upgrading

Publish the new `purser.wasm` to the same database name. Never pass `--delete-data` / `-c`: it deletes the ledger. New tables are created automatically; the revenue history is rebuilt from the ledger by the worker within a minute.
