# Getting started

This page takes you from nothing to a verified test purchase. Do the four numbered sections in order.

## Before you start

You need:

- **A store developer account**: Google Play and / or App Store Connect, with the app and its products created.
- **A place to run the module.** You choose it in step 1: maincloud (a SpacetimeDB account, nothing to host), your own server with Docker, or a build from source.
- **For a Unity game**: Unity IAP 5.4 or later.
- **Optional**: Python 3.9+, to set up from the terminal with `seed.py` instead of the admin panel.

## 1. Publish the module

Pick one of the three ways below. Maincloud is the fastest.

### On maincloud

With a SpacetimeDB account there is nothing to host. The module is live a few seconds after the publish.

1. Install the [SpacetimeDB CLI](https://spacetimedb.com/install) 2.16.3 or newer.
2. Sign in with `spacetime login`.
3. Download the module from the [latest release](https://github.com/yevhenii-sir/purser/releases/latest) and publish it. Run these commands outside a SpacetimeDB project folder:

    ```bash
    curl -LO https://github.com/yevhenii-sir/purser/releases/latest/download/purser.wasm
    spacetime publish --server maincloud --bin-path purser.wasm <your-billing-db>
    ```

4. Open the admin panel at `https://maincloud.spacetimedb.com/v1/database/<your-billing-db>/route/admin`.
5. Sign in with your token. `spacetime login show --token` prints it.

!!! tip "Check the download"
    Run `gh attestation verify purser.wasm -R yevhenii-sir/purser` with the [GitHub CLI](https://cli.github.com). It checks that the file was built by this repository's release workflow from a tagged commit, and not changed after the build. Releases up to 0.13.0 have no attestation.

Coworkers can later sign in with Google, GitHub or Discord through SpacetimeAuth instead of tokens. See [sign-in with a provider](self-hosting.md#sign-in-with-google-github-and-other-accounts).

### On your own server (Docker)

1. Run these commands on the server. In `.env`, set `BILLING_DB` and `BILLING_DOMAIN`. The last command adds HTTPS with Caddy.

    ```bash
    git clone https://github.com/yevhenii-sir/purser.git
    cd purser/deploy/docker
    cp .env.example .env        # set BILLING_DB and BILLING_DOMAIN
    docker compose up -d
    ./publish.sh
    docker compose --profile proxy up -d    # HTTPS with Caddy
    ```

2. Open the admin panel at `http://localhost:3000/v1/database/<your-billing-db>/route/admin` on the server. From another machine, open it through a tunnel: `ssh -N -L 3000:localhost:3000 user@stdb.example.com`.
3. Sign in with the token from `./publish.sh --token`.

Details: [deploy/docker/README.md](../deploy/docker/README.md) and [Self-hosting](self-hosting.md).

### From source

Build the module and publish it:

```bash
cd server
cargo build -p purser --target wasm32-unknown-unknown --release
spacetime publish --bin-path target/wasm32-unknown-unknown/release/purser.wasm <your-billing-db>
```

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the toolchain and the tests.

!!! note "A database created in the dashboard"
    If you created the database in the SpacetimeDB dashboard before publishing, the admin panel first asks you to **finish the installation**. One click registers you as the owner admin and starts the worker. This step is needed because an update does not run the module's `init`.

## 2. Add an app

An app groups the builds of one game, one build per store. Builds of one game share purchases and entitlements.

1. In the admin panel, open **Apps → Add an app**.
2. Enter the Android package and / or iOS bundle id.
3. Choose how rewards reach the game ([Game integration](game-integration.md)):
    - a reducer in your SpacetimeDB game database;
    - a signed webhook;
    - the client itself, for games without a backend.
4. Upload the store keys ([Store setup](store-setup.md)).
5. Import products from Google Play, or enter them.

The last step checks everything. It shows the URLs to paste into Play Console (RTDN) and App Store Connect (Server Notifications). The app's **Setup** tab repeats the check until it is green.

### Or from the terminal

Run `python seed.py --init` in `server/` of a clone of the repository. It needs Python 3.9+ and no packages. It asks the same questions as the wizard, then:

- writes the answers to `server/secrets/` (git-ignored);
- generates the notification secrets;
- uploads everything;
- offers the Google Play catalog import;
- runs the setup check;
- prints the URLs for the store consoles and the values for Unity's `BillingConfig`.

!!! tip "Configuration as code"
    The same configuration can live in JSON files. `server/seed.py` uploads them and also runs the setup check:

    1. Copy `server/seed.example` to `server/secrets` (git-ignored).
    2. Fill it in.
    3. Run `python seed.py --check`.

    `python seed.py --export backup` writes the configuration of a database to files in the same format. Keep them in git, or use them to copy the configuration to another database. The export covers apps, products, integrations, paywalls, placements, alerts and settings. For keys it writes the names of the keys that are set, never their values.

## 3. Connect the game

### Unity

1. Add the package and Unity IAP 5.4 or later to `Packages/manifest.json`:

    ```json
    "com.yevhenii.purser": "https://github.com/yevhenii-sir/purser.git?path=/unity/com.yevhenii.purser",
    "com.unity.purchasing": "5.4.4"
    ```

    Without Unity IAP, the Google Play / App Store adapter is not compiled, and `BillingService` asks for another store adapter.

2. Create the config: **Tools → Purser → Create Config**.
3. Fill in the config from the admin panel, or the admin panel from the config (see below).

The config and the admin panel exchange one JSON, in either direction. It holds the server address, the builds by store, and the products with their store-specific ids.

- **Admin panel first:** copy the *Unity config* from the last step of the setup wizard. Paste it into the config inspector.
- **Game first:** copy the JSON from the config inspector. On the first step of **Apps → Add an app**, press **Paste from Unity**. It fills in the builds and the products.

When you add products in the game later, import them with **Import from Unity** on the app's Products tab. It shows what is new or changed. It adds nothing for builds the app does not have yet.

The [Unity package README](../unity/com.yevhenii.purser/README.md) covers the API (`PurchaseAsync`, `IsActive`, `RestoreAsync`) and the DI adapters.

### Other engines

Call the [HTTP API](api.md) after each purchase. Follow the client rules in [Game integration](game-integration.md#client-integration).

## 4. Test a purchase

1. Enable **Sandbox purchases** on the app's Delivery tab.
2. Buy with a license tester (Google) or a sandbox account (Apple).
3. Open the app's **Purchases** tab. The purchase goes `verified` → `granted` → `completed`.

## What you have now

- The Purser module running on your database, with its admin panel.
- An app with its builds, store keys and products, checked on the **Setup** tab.
- A game that sends its purchases to Purser.
- A test purchase that reached `completed`.

## Upgrading

Publish the new `purser.wasm` to the same database name. With Docker, run `./publish.sh` again.

!!! warning
    Don't use `--delete-data` unless the release notes say the release needs it. It deletes the purchase ledger.
