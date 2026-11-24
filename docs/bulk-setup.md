# Bulk setup: hundreds of apps

The admin page and `seed.py --init` set up one app at a time. For a catalog of many games, three
scripts in `server/` do the same from tables:

| Step | Script | What it does |
|---|---|---|
| Builds and products | `bulk_setup.py` | creates or updates builds from `apps.csv` and products from `products.csv`, and adds the products Google Play and the App Store already list (`--from-stores`) |
| Google Play | `setup_google.py` | the Google Cloud side for every Google Play build: service account and key, Pub/Sub topic, RTDN subscriptions |
| Check | `seed.py --check-all` | the setup check of every build, as one table |

All of them run from `server/` with the database in `spacetime.json` (or `--server` / `--database`)
and the token of `spacetime login` or `PURSER_TOKEN`; see [server/README.md](../server/README.md).
They are safe to run again: what is done already is left as it is.

## Order

1. **Developer accounts and their keys.** A build reads its store keys from its developer account
   (the `account` column) unless it has its own. Store keys are per account, not per game: one
   App Store Connect key, one Amazon shared key, one Steam publisher key per account. Put them in
   `credentials.json` with `"scope": "<account>"` and load it with `python seed.py --dir secrets`
   (see [store-setup.md](store-setup.md) for where each key comes from). The Google Play service
   account key comes from step 4. Delivery keys (`spacetime_token`, `webhook_secret`) also go per
   account when the games share them.
2. **Builds**: `python bulk_setup.py --apps apps.csv --dry-run`, read the diff, then without `--dry-run`.
3. **Products**: `python bulk_setup.py --apps apps.csv --products products.csv --from-stores --dry-run`,
   then without `--dry-run`. Steps 2 and 3 can be one run.
4. **Google Play**: `python setup_google.py --project <your-cloud-project> --dry-run`, then without
   `--dry-run`; then in Play Console invite the service account and set the RTDN topic (the script
   prints what is left; once per developer account, not per app).
5. **App Store**: the App Store Server Notifications URL of each app (the app's Setup tab sets it
   through the App Store Connect API, or paste `urls.notify` of `seed.py --check-all --json` into App Store Connect).
6. **Check**: `python seed.py --check-all` and fix what fails; run it again until it exits with 0.

## `apps.csv`

One build per line: a game on Google Play and the App Store is two lines with the same `game_key`.

```csv
game_key,platform,store,app_id,account,delivery
my-game,android,,com.example.mygame,my-studio,none
my-game,ios,,com.example.mygame,my-studio,none
my-game,amazon,,com.example.mygame.amazon,my-studio,none
other-game,android,,com.example.othergame,publisher,webhook
```

| Column | |
|---|---|
| `app_id` | required. The store's id where the store API needs it (Android package, iOS bundle id), any unique id otherwise (`com.example.mygame.amazon`); no `/` or `:` |
| `platform` | required: `android`, `ios`, `amazon`, `steam` or `web` |
| `store` | empty = the platform's default store; `web` builds name `stripe` or `xsolla` |
| `game_key` | the app the build belongs to; empty = the `app_id` |
| `account` | the developer account whose keys the build uses; empty = `default` |
| `delivery` | `none`, `spacetimedb` or `webhook`; required for a new build |

Optional columns, as in `admin_upsert_game_app`: `spacetime_url`, `spacetime_db`, `grant_reducer`,
`revoke_reducer` (needed by `spacetimedb` delivery), `webhook_url` (`webhook` delivery, https),
`allow_sandbox`, `enabled`, `client_consumes` (`true` / `false`), `apple_refund_mode`
(`off`, `inform`, `prefer`), `client_auth` (`open`, `signed`), `sns_topic_arn`.

## `products.csv`

One product of one build per line. A game sold in two stores lists the product for each build;
`catalog_id` is the game's own id of the product (the Unity config's product `id`), the same in
every store, and may be left empty while the store id is the same everywhere.

```csv
app_id,product_id,kind,catalog_id
com.example.mygame,gems_100,consumable,
com.example.mygame,remove_ads,non_consumable,
com.example.mygame,vip_month,subscription,
com.example.mygame.ios,com.example.mygame.gems100,consumable,gems_100
```

| Column | |
|---|---|
| `app_id` | required: a build of `apps.csv` or one on the server |
| `product_id` | required: the store's id |
| `kind` | `consumable`, `non_consumable` or `subscription`; required for a new product unless `--from-stores` finds it in the store |
| `catalog_id` | the game's id of the product; one per build |

Optional columns: `price` (`0.99`) or `price_micros` (`990000`) with `currency` (`USD`), `title`,
`description`, `period` (`P1W`, `P1M`, `P3M`, `P6M`, `P1Y`) and `apple_group` for subscriptions,
`access` (access levels, comma-separated inside quotes: `"premium,no_ads"`).

## How the files are read

- `.csv` files have a header line; blank lines and lines starting with `#` are skipped. Excel's
  "CSV UTF-8" works. A `.json` file is a list of objects with the same keys.
- **An empty cell keeps what the server has**; a new row gets the default. So a file with only
  `app_id,product_id,price,currency` changes prices and nothing else, and running the same files
  twice changes nothing the second time.
- Every row is checked before anything is written: platforms, stores and kinds as the server
  accepts them, ids, prices, periods, duplicate builds, duplicate products, a catalog id used twice
  in a build, products of builds that exist nowhere, a new build without delivery. Errors name the
  file and line (`apps.csv:12: store apple is not available on android`) and nothing is written.
- `--dry-run` prints the diff, new / changed (with each changed field) / unchanged (`-v` lists the
  unchanged rows too), and writes nothing.
- Builds are written one call each; products 100 per `admin_upsert_products` call (`--batch`). A
  call the server refuses is reported and the rest goes on; a refused batch is sent again one product
  at a time to find the row. The exit code is 1 when anything failed: fix it and run the same
  command again.

## `--from-stores`

For every Google Play and App Store build of `apps.csv` (every such build on the server when
`--apps` is not given), `bulk_setup.py` lists the store catalog with `admin_list_store_products`,
4 builds at a time (`--jobs`), and adds the products the server does not have yet:

- Google Play: one-time products and subscriptions with the US price (else the first one listed)
  and the subscription period. Google Play does not say whether a one-time product is consumed:
  they come in as `consumable`. List the non-consumables in `products.csv` (`kind` only), which wins
  over the store.
- App Store: in-app purchases with their kind and subscriptions with their period, without prices
  (the App Store Connect API lists them per territory); needs the Team key secrets
  `apple_connect_key_id` and `apple_connect_private_key_p8` of the build's account.
- Products the server has already are left as they are; a store product whose id is the catalog id
  of another product of the build is skipped with a note.
- A build whose catalog could not be listed (keys missing, no access) is reported; the other builds
  go on. In a dry run, builds that do not exist yet are listed on the real run.

## `setup_google.py` for many builds

The module sets up at most 20 builds per call. The script groups the Google Play builds by developer
account (the key of each account goes to that account unless `--scope` names one) and sends them 20 at
a time, all on the same Pub/Sub topic (`--topic`, default `play-billing`). `--new-key` makes one new key
per account. A call that fails stops the run and names the builds not set up yet; steps done already
are skipped on the next run.

## `seed.py --check-all`

Reads every build with the owner's `/sql` (other admins: `admin_select`), runs `admin_check_app` for
every enabled build, 4 at a time (`--jobs`), and prints one line per build (OK / WARN / FAIL with the
number of checks of each level), the totals, then each failed check with its fix. `--offline` checks
the configuration without calling the stores, `--json` prints the full reports. The exit code is 1
when a build failed or could not be checked, so it fits in CI or a cron job.

## Notes for hundreds of apps

- Keep the tables in git (no secrets in them) and treat them as the source of truth: run
  `bulk_setup.py --dry-run` in review, the real run after merge.
- Share keys through developer accounts rather than per build: 500 builds on 3 accounts need 3 sets
  of store keys, and a rotated key is changed once.
- Store APIs have quotas. `--jobs` (catalog listing, checks) defaults to 4; raise it carefully, lower
  it when stores answer 429.
- The online check of an App Store build asks Apple for a test notification; `--offline` skips the
  store calls when only the configuration changed.
- Paywalls, placements, integrations and alerts are per app and still go through `seed.py` files
  or the admin page.
