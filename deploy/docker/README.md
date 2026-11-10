# Self-hosting with Docker

A SpacetimeDB server with the billing module on one machine, plus an optional [Caddy](https://caddyserver.com) proxy with automatic HTTPS. The general rules (which paths to expose, the admin page through a tunnel, delivery to games on the same server) are in [docs/self-hosting.md](../../docs/self-hosting.md); this folder puts them into a `docker compose` setup.

| File | What |
|---|---|
| `docker-compose.yml` | `spacetimedb` (the server, `clockworklabs/spacetime:v2.14.1`), `cli` (one-shot SpacetimeDB CLI used by `publish.sh`), `caddy` (optional proxy, profile `proxy`) |
| `publish.sh` | Downloads `purser.wasm` from GitHub Releases (or takes a local build) and publishes it |
| `Caddyfile` | HTTPS for `BILLING_DOMAIN`, only the public paths are proxied |
| `.env.example` | Settings: database name, domain, versions |

Requirements: Docker with Compose v2 (`docker compose`), `curl`, a POSIX shell (Linux, macOS, WSL or Git Bash). The SpacetimeDB CLI is **not** needed on the host: it runs in the `cli` container.

## Start

```bash
cd deploy/docker
cp .env.example .env        # set BILLING_DB; BILLING_DOMAIN for HTTPS
docker compose up -d        # SpacetimeDB on 127.0.0.1:3000
./publish.sh                # download the latest release and publish it to BILLING_DB
```

`publish.sh` options:

```bash
./publish.sh --release v0.1.0                     # a specific release instead of the latest
./publish.sh --wasm ../../server/target/wasm32-unknown-unknown/release/purser.wasm   # a local build
./publish.sh other-billing-db                     # another database name than BILLING_DB
./publish.sh --token                              # print the owner token
```

On the first run the CLI gets an identity **from your server** (not from spacetimedb.com) and publishes the module as that identity, which makes it the database owner and the first admin. The identity's token is kept in the `stdb-cli` volume.

## Admin page

The server port is bound to `127.0.0.1` only. On the server itself, or through an SSH tunnel from your computer:

```bash
ssh -N -L 3000:localhost:3000 user@stdb.example.com
```

open `http://localhost:3000/v1/database/<your-billing-db>/route/admin` and sign in with the owner token:

```bash
./publish.sh --token        # same as: docker compose run --rm -T cli login show --token
```

Then set Settings → Server → **Public address** to `https://stdb.example.com/v1/database/<your-billing-db>/route`, so the setup check and the **Add an app** wizard show the URLs the stores and the Unity config need.

The CLI works through the same tunnel or container for everything else:

```bash
docker compose run --rm cli logs --server http://spacetimedb:3000 <your-billing-db>
docker compose run --rm cli sql --server http://spacetimedb:3000 <your-billing-db> "SELECT id, status FROM purchase"
```

If the SpacetimeDB CLI is installed on your computer, you can use it over the tunnel too: `spacetime server add selfhosted --url http://localhost:3000`, then log in with the owner token (`spacetime login --token <token>`) so the commands run as the owner; `seed.py` and `migrate.py` take `--server selfhosted`.

## HTTPS with Caddy

1. Point a DNS record for `BILLING_DOMAIN` at the host and open ports 80 and 443.
2. `docker compose --profile proxy up -d`

Caddy gets a Let's Encrypt certificate and forwards only the paths from the [allowlist](../../docs/self-hosting.md#expose-only-what-clients-and-stores-need): the billing routes (`/v1/database/*/route/*`), `call`, `subscribe`, `schema`, `/v1/identity*` and `/v1/ping`. Everything else, including `publish`, `sql` and `logs`, answers 404. The admin page is not served at the public address either; use the tunnel.

The URLs for the stores and clients are then:

| | URL |
|---|---|
| Billing API (Unity config `serverBaseUrl`) | `https://stdb.example.com/v1/database/<your-billing-db>/route` |
| Google RTDN push endpoint | `https://stdb.example.com/v1/database/<your-billing-db>/route/notify/google?secret=<google_rtdn_secret>` |
| App Store Server Notifications V2 | `https://stdb.example.com/v1/database/<your-billing-db>/route/notify/apple` |
| Health check | `https://stdb.example.com/v1/database/<your-billing-db>/route/healthz` |

Without Caddy, put any TLS proxy in front of `127.0.0.1:3000` with the same allowlist (nginx example in [docs/self-hosting.md](../../docs/self-hosting.md)).

## Upgrading

- **Module:** run `./publish.sh` again (or with `--release <tag>`). It publishes to the same database and never passes `--delete-data`, so the ledger stays.
- **SpacetimeDB:** change `SPACETIMEDB_VERSION` in `.env` (to the version the module release names), then `docker compose pull && docker compose up -d`.

## Data and backups

| Volume | Holds | Losing it means |
|---|---|---|
| `purser_stdb-server` | databases (`~/.local/share/spacetime/data`) and the server's token signing keys (`~/.config/spacetime/id_ecdsa*`) | the purchase ledger and every issued token |
| `purser_stdb-cli` | the CLI login: the **owner token** of the billing database | no more publishing or owner-only actions; keep a copy of the token (`./publish.sh --token`) in your password manager, restore it with `docker compose run --rm cli login --token <token>` |
| `purser_caddy-data` | certificates | Caddy requests new ones |

Back up with the server stopped, for example:

```bash
docker compose stop spacetimedb
docker run --rm -v purser_stdb-server:/v -v "$PWD":/backup alpine tar czf /backup/stdb-server.tgz -C /v .
docker compose start spacetimedb
```

To move to another server instead, see [moving the ledger](../../docs/migrating-from-adapty.md#moving-the-ledger-to-another-server).

## Delivery to games on the same server

The module cannot call private addresses, so delivery to a game database on this server goes through the public hostname: allow it in Settings → Server → SpacetimeDB hosts (`stdb.example.com`) and keep the `call` path in the proxy (it is in the Caddyfile).

## Troubleshooting

- `./publish.sh` says the release was not found: no release has been published yet, or the tag is wrong. Build the module yourself (`cargo build -p purser --target wasm32-unknown-unknown --release` in `server/`) and pass `--wasm`.
- `docker compose ps` shows `spacetimedb` unhealthy: `docker compose logs spacetimedb`.
- The admin page signs in but says you are not an admin: you signed in with a token other than the publisher's. Use `./publish.sh --token`.
