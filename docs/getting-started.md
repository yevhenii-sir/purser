# Getting started

From nothing to a verified test purchase. You need a Google Play and / or App Store Connect developer account with the app and its products created.

## 1. Publish the module

### On maincloud

Install the [SpacetimeDB CLI](https://spacetimedb.com/install) 2.14.1 or newer and sign in with `spacetime login`. Then download the module from the [latest release](https://github.com/yevhenii-sir/purser/releases/latest) and publish it (outside a SpacetimeDB project folder):

```bash
curl -LO https://github.com/yevhenii-sir/purser/releases/latest/download/purser.wasm
spacetime publish --server maincloud --bin-path purser.wasm <your-billing-db>
```

The admin panel is at `https://maincloud.spacetimedb.com/v1/database/<your-billing-db>/route/admin`; sign in with your token (`spacetime login show --token`).

### On your own server (Docker)

```bash
git clone https://github.com/yevhenii-sir/purser.git
cd purser/deploy/docker
cp .env.example .env        # set BILLING_DB and BILLING_DOMAIN
docker compose up -d
./publish.sh
docker compose --profile proxy up -d    # HTTPS with Caddy
```

The admin panel is at `http://localhost:3000/v1/database/<your-billing-db>/route/admin` on the server (or through `ssh -N -L 3000:localhost:3000 user@stdb.example.com`); sign in with the token from `./publish.sh --token`. Details: [deploy/docker/README.md](../deploy/docker/README.md) and [Self-hosting](self-hosting.md).

### From source

```bash
cd server
cargo build -p purser --target wasm32-unknown-unknown --release
spacetime publish --bin-path target/wasm32-unknown-unknown/release/purser.wasm <your-billing-db>
```

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the toolchain and the tests.

If you created the database in the SpacetimeDB dashboard before publishing, the admin panel first asks you to **finish the installation**: one click registers you as the owner admin and starts the worker (an update does not run the module's `init`).

## 2. Add an app

Admin panel → Apps → **Add an app**:

1. Enter the Android package and / or iOS bundle id. Builds of one game share purchases and entitlements.
2. Choose how rewards reach the game: a reducer in your SpacetimeDB game database, a signed webhook, or the client itself for games without a backend ([Game integration](game-integration.md)).
3. Upload the store keys ([Store setup](store-setup.md)).
4. Import products from Google Play or enter them.

The last step checks everything and shows the URLs to paste into Play Console (RTDN) and App Store Connect (Server Notifications). The app's **Setup** tab repeats the check until it is green.

**Or from the terminal:** `python seed.py --init` (in `server/` of a clone of the repository, Python 3.9+, no packages) asks the same questions, writes the answers to `server/secrets/` (git-ignored), generates the notification secrets, uploads everything, offers the Google Play catalog import, runs the setup check and prints the URLs for the store consoles and the values for Unity's `BillingConfig`.

!!! tip "Configuration as code"
    The same configuration can live in JSON files and be uploaded with `server/seed.py`, which also runs the setup check: copy `server/seed.example` to `server/secrets` (git-ignored), fill it in and run `python seed.py --check`. `python seed.py --export backup` writes the configuration of a database (apps, products, integrations, paywalls, placements, alerts, settings; the names of the keys that are set, never their values) to files in the same format, to keep it in git or to copy it to another database.

## 3. Connect the game

**Unity:** add the package to `Packages/manifest.json`, create the config (Tools → Purser → Create Config) and press **Paste config from admin panel**:

```json
"com.yevhenii.purser": "https://github.com/yevhenii-sir/purser.git?path=/unity/com.yevhenii.purser"
```

The [Unity package README](../unity/com.yevhenii.purser/README.md) covers the API (`PurchaseAsync`, `IsActive`, `RestoreAsync`) and the DI adapters.

**Other engines:** call the [HTTP API](api.md) after each purchase and follow the client rules in [Game integration](game-integration.md#client-integration).

## 4. Test a purchase

Enable **Sandbox purchases** on the app's Delivery tab, buy with a license tester (Google) or a sandbox account (Apple), and open the app's **Purchases** tab: the purchase goes `verified` → `granted` → `completed`.

## Upgrading

Publish the new `purser.wasm` to the same database name (`./publish.sh` again with Docker). Never use `--delete-data`: it deletes the purchase ledger.
