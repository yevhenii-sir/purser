# Revenue analytics

The admin panel's **Revenue** page shows what the stores paid, in USD. It works from the purchase
ledger, so no analytics SDK is needed. The same page is on every app's **Revenue** tab, limited to
that app's builds.

The page shows:

- net revenue, estimated proceeds and refunds;
- purchases;
- subscriptions: new, trials, trial conversion, renewals, churn;
- active subscriptions and MRR;
- a daily chart;
- breakdowns by app, product, currency, billing country and Apple Ads campaign;
- payer cohorts.

## Analytics modes

The server setting `analytics` says how much the server counts. Set it in the admin page → Server →
Analytics, or with `admin_set_setting("analytics", …)`.

| Mode | What runs | What you get |
|---|---|---|
| `full` (default) | Every purchase change adds to the daily totals (a few row writes). When the Revenue page asks for the full report, it is counted **in the background**, a batch of purchases per scheduled call. | Everything on this page. |
| `light` | The daily totals only. No report reads the purchase ledger. | Totals, the daily series, products, countries, subscription snapshots, the revenue tree, business alerts, `/metrics`. Payer cohorts, LTV, ARPPU, Apple Ads campaigns, trials and the subscriptions running now stay empty. Install cohorts have their installs only. |
| `off` (large games) | Only what purchases need: no revenue aggregates, no subscription snapshots, no reports. The ledger still logs what each change would count. Each build's daily sales are still counted for alerts (a few rows per build and day, kept two months). | No revenue pages. The reports answer `ERR_ANALYTICS_OFF`. Business alerts and `/metrics` keep working. |

With `off`:

- Count revenue in your attribution or analytics tools. Tenjin, AppsFlyer, Adjust, ... get every
  purchase, renewal and refund as an
  [integration](../server/README.md#integrations-ga4-appsflyer-adjust-tenjin-appmetrica-amplitude-mixpanel-meta-webhook-cloud-storage)
  event.
- Or count it from the Cloud Storage export to BigQuery.
- When you turn analytics on again, the totals are rebuilt from the ledger, the time it was off
  included.

### Why the full report is counted in the background

SpacetimeDB computes one call of a database at a time. A report that read the ledger for seconds in
one call would hold every purchase and request for that long.

In the background, each call reads one batch: 2,000 purchases of the range, then the whole history of
200 of the payers and trial subscriptions they name. It adds what it counts to a few kilobytes kept
with the job. Other calls run in between. The last call only turns that into the report and keeps it
for an hour. No call reads more as the ledger grows.

While the report is counted, the page shows the previous report, or waits for the first one.

## What is counted

| Metric | When | Day |
|---|---|---|
| Gross revenue, purchases, new subscriptions, renewals | the first time a store purchase is granted | the day the purchase was verified |
| Free trials | the first grant of a free-trial period (no money) | the day the trial started |
| Trial conversions | the first paid period of a subscription after a free trial (also a new subscription, not a renewal) | the day it was verified |
| Refunds | the store reports a refund of a granted purchase | the day it is reported |
| Expirations, transfers | a paid subscription expires / an owned product moves to another user | the day it happens |
| Trial expirations | a free trial ends without a payment (not an expiration) | the day it happens |
| Active subscriptions, MRR | Paid subscriptions running, renewals counted once. Free trials running are counted apart (`trials_now`). | snapshot taken daily |

- **Manual grants** are not revenue.
- **Sandbox purchases** are excluded unless *Include sandbox* is on.
- **Amounts** are what the buyer paid: before the store's commission, and with the tax the price
  includes.
    - The App Store reports the price.
    - For Google Play, the server reads the order of each new purchase and subscription period through
      the Orders API: the total with tax, the tax and the buyer's country. The service account needs
      *View financial data*.
    - Without that, Google one-time products fall back to the client's price (`price_micros` in
      `/verify`, when believable) or the build's product price. Subscriptions fall back to the renewal
      price Google reports.
    - Set prices on Android products. The setup check warns about missing ones. Each build has its own.
- **Partial refunds** keep the purchase and take their amount out of revenue. They are:
    - Stripe and Xsolla partial refunds;
    - Google Play refunds of some units of a multi-quantity purchase (their share of the price);
    - App Store prorated refunds (`revocationType` `REFUND_PRORATED`). The purchase is revoked, and the
      refund counts `revocationPercentage` of the price.
- **A free-trial period** is recorded with price 0, so integrations get no revenue for it either. Its
  events carry `trial`, and the first payment after it `trial_conversion`; integrations can name them
  apart from paid periods (`trial_start`, `trial_converted`, ... in `event_names`). Google
  reports the price of the next renewal for it, which is not what the buyer paid. Trials are detected
  from Google's `lineItems[].offerPhase.freeTrial` (subscriptionsv2) and Apple's `offerDiscountType` =
  `FREE_TRIAL` (older Apple payloads: introductory offer with price 0).
- **MRR** divides each subscription's price by its billing period. The period is guessed from the paid
  period (weekly, monthly, quarterly, half-yearly, yearly).
- **Days** are UTC.

## Billing country

Every purchase keeps the buyer's billing country (ISO 3166-1 alpha-2). It comes from:

- Google's `regionCode` (one-time products and subscriptions);
- Apple's `storefront` (alpha-3, converted).

Purchases recorded before this was added show as *Unknown*. **By country** lists gross, refunds, net,
estimated proceeds, payments and trials per country.

## Proceeds (estimate)

Proceeds = net revenue − estimated store fee. The fee is a commission percent applied at report time,
so changing it needs no rebuild.

- **Default commission**: each store's usual fee.
    - 15% for Google Play and the App Store (their small business rates).
    - 20% Amazon Appstore, 30% Steam.
    - About 3% Stripe and 5% Xsolla (payment fees).

    Setting a server-wide percent replaces it for every store.
- **Per build**: a different percent for some apps. For example 30% for a big title, or 15% for the
  Android build because Google Play takes 15% on all subscriptions.
- **Subscriptions after 12 paid months**: both stores charge 15% on a subscription after its first
  year of paid service. A renewal recorded 365 days or more after the subscription's first paid period
  uses `min(app percent, this percent)` (default 15).
- **Refunds** return the commission too, because the fee is taken on net revenue.

!!! note
    Revenue includes the taxes in the price (VAT, digital services taxes). Real payouts are lower in
    countries where the store withholds them. The `net` amount of integration events deducts the tax
    Google reported, or an estimate from the billing country.

To change the fee, open Revenue → **Store fee**, or call `admin_set_setting` with these server
settings:

- `store_commission_percent` (`"30"`);
- `store_commission_after_year_percent` (`"15"`);
- `store_commission_by_app` (`{"com.example.mygame": 30}`).

An empty value restores the default.

## Subscriptions

The trial funnel and paid retention are counted apart.

- **New subscriptions** = paid subscriptions started, either paid at once or with the first payment
  after a free trial. That first payment after a trial is counted in `trial_conversions`. The stores
  report it as a renewal.
- **Trials started** counts free trials.
- **Trial expirations** counts the trials that ended unpaid.
- **Trial → paid**: of the free trials started in the period, the share that has a later paid period
  of the same subscription. Trials still running (not expired, not paid yet) are left out of the rate.
- **`trial_conversion_rate`** (in `subscriptions` and per month) is the same idea by the day things
  happened: trials that ended paid ÷ trials that ended (paid or not) in the period.
- **Renewal rate** = renewals ÷ (renewals + expirations). Of the paid periods that ended, it is the
  share that continued. Neither the first payment after a trial nor a trial that ended unpaid is in it.
- **Churn** = expirations ÷ paid subscriptions active at the first daily snapshot of the period (or
  month), per 30 days.
    - Running trials are not active subscriptions, and an unpaid trial is not churn.
    - The snapshots start on the day the analytics version was installed, so churn is empty before that.
- **Subscriptions by month** shows these per calendar month.
- **Active subscriptions** also says how many of them:
    - will not renew (auto-renew off): `auto_renew_off_now`;
    - had a renewal payment fail: `billing_issue_now`;
    - are in the store's grace period after it: `in_grace_now`;
    - belong to players who have not accepted a price increase yet: `price_consent_needed_now`. Those end
      with their period unless the players accept.

    The full report counts the running subscriptions in its background job, a batch per scheduled
    call. The daily snapshot walks them the same way.
- **A subscription replaced by another** (upgrade, downgrade) is not an expiration.

## Revenue tree

The **Revenue tree** on the Revenue page (and on an app's Revenue tab) shows where net revenue comes
from. It reads left to right:

1. paywall views and trials started;
2. purchases, new subscriptions, renewals and expirations;
3. one-time sales and subscription revenue;
4. gross revenue and refunds;
5. net revenue.

Every number has its change against the previous period of the same length. The change is green when
it moved the right way (down for refunds and expirations).

The labels on the links are ratios:

- **paywall views → purchases / new subscriptions**: payments per view. Hidden when there are more
  payments than views, i.e. most payments happen outside paywalls.
- **trials → new subscriptions**: trial conversions ÷ trials started in the period. Conversions of
  trials started earlier count too, so it is a rough rate. **Trial → paid** above is the exact one.
- **purchases → one-time sales**: the average one-time purchase.
- **renewals → subscriptions**: renewals ÷ (renewals + expirations) of paid periods.
- **expirations → active subscriptions**: churn per 30 days.
- **one-time sales / subscriptions → gross**: their share.
- **refunds → net**: refunds ÷ gross.

More about the tree:

- Active subscriptions and MRR are those of the last daily snapshot of the period.
- Subscription nodes are hidden when the apps have no subscriptions. The paywall node is hidden when
  no views were reported.
- Paywall views are not split by environment (sandbox views count).
- The tree reads two summary reports (the period and the one before), so it is as cheap as the
  Overview.

## Payer cohorts

Payers are grouped by the month of their first payment, for cohorts that start in the selected period.

- A payer is a player of one app with a granted, non-trial store purchase.
- The same id in two apps is two payers.
- Family Sharing purchases are not payments.

Each cell is the cohort's cumulative net revenue per payer by month since the first payment (M0 = that
month), up to today. Refunded purchases count as 0. Cohorts are computed from the ledger when the
report is opened.

## Install cohorts

Players are grouped by the month the server first heard from them in a build. Any client call counts:
`/entitlements` at start-up, `/paywall`, `/verify`. This is the profile's `first_seen`, kept in
`install_daily` per build and day.

Per cohort, the full report (`install_cohorts`) shows:

- the installs;
- how many of those players paid (`payer_rate`);
- their cumulative net revenue by month since the install, in total and per installed player.

Compare the revenue per install with what a campaign paid per install.

- A payer's install is the first time they were seen in a build of the app they paid in.
- Installs count from the version that added them on.
- A player whose profile was erased (retention, a deletion request) counts as new when they come back.
- A purchase of a player the server never heard from (an import) belongs to no install cohort.

## ARPPU and LTV

- **ARPPU**: net revenue of the period's paid purchases ÷ the users who paid in the period (`arppu`).
- **LTV, N months**: cumulative net revenue per payer N months after the first payment (1, 3, 6, 12)
  (`ltv`). It covers the payer cohorts of the period whose first N months are over. The current month
  does not count as over. LTV is empty until a cohort is old enough.

## Apple Ads

**By Apple Ads campaign** shows payers, payments, trials, gross, refunds and net per campaign, for
purchases made in the period. It works when the Unity package resolves the install's Apple Ads
attribution (`asa_campaign_id`, kept per user in `user_attribution`).

Only campaign ids are known (no names). The breakdown is computed from the ledger when the report is
opened.

## Currencies

Every amount is kept in its own currency. It is converted to USD with the European Central Bank
reference rate of that day ([frankfurter.dev](https://frankfurter.dev), no key needed). Weekends use
Friday's rate. The worker fetches the latest rates every 6 hours and backfills history as far back as
the first sale.

The ECB publishes about 30 currencies. Others (UAH, VND, …) are listed as *No rate for …*. They are
left out of USD totals until you set a manual rate.

- **Set a manual rate**: Revenue → By currency → **Set rate**, or `admin_set_fx_override("UAH", 41.5)`.
  The rate is in units per 1 USD.
- **Go back to the ECB rate**: `admin_delete_fx_override("UAH")`.
- **Fetch the latest ECB rates now**: **Rates** on the Revenue page (`admin_refresh_fx`).

A currency the ECB stopped publishing for more than two weeks is converted at its last rate and listed
under `fx.stale`. Totals per currency are always shown in the original currency.

## Data and API

The tables:

- `revenue_daily`: one row per day × app × product × environment × currency, updated in the same
  transaction as the ledger. Besides the counts and amounts it keeps trials, trial conversions and the
  gross / refunds of subscription periods after the first paid year.
- `revenue_country_daily`: the same per day × app × billing country × environment × currency.
- `subscription_daily`: paid subscriptions active per day, free trials running (`trials`) and the MRR
  of the paid ones.
- `fx_day`, `fx_override`: exchange rates.
- `purchase.country`, `purchase.trial`: billing country and free-trial flag of each ledger row.

### The full report

`admin_revenue_stats(query_json)` returns the report as JSON. The query:
`{"from": "2026-10-01", "to": "2026-10-31", "game_key": "my-game", "app_ids": [], "include_sandbox": false}`.
All fields are optional. The default is the last 30 days, all apps.

Main fields:

| Field | Content |
|---|---|
| `totals`, `series[]`, `by_app[]`, `by_product[]`, `by_country[]` | `gross_usd_micros`, `refunded_usd_micros`, `net_usd_micros`, `store_fee_usd_micros`, `proceeds_usd_micros`, `purchases`, `new_subscriptions`, `renewals`, `refunds`, `transfers`, `expirations`, `trials`, `trial_conversions`, `trial_expirations` (`by_app` also `commission_percent`) |
| `by_currency[]` | amounts in the original currency and in USD, `converted` |
| `proceeds` | the commission used: `percent`, `after_year_percent`, `by_app`, `estimated: true` |
| `subscriptions` | `active_now` (paid), `trials_now`, `mrr_usd_micros`, `series[]` (`day`, `active`, `trials`, `mrr_usd_micros`), `new_subscriptions`, `trials_started`, `trial_conversions`, `trial_expirations`, `trial_conversion_rate`, `renewals`, `expirations`, `active_at_start`, `churn_rate`, `renewal_rate`, `monthly[]` (the same per month) |
| `trials` | `started`, `converted`, `pending`, `conversion_rate` (trials started in the period) |
| `cohorts[]` | `cohort` (`YYYY-MM`), `payers`, `revenue_usd_micros[]` and `per_payer_usd_micros[]` (cumulative by month since the first payment) |
| `ltv[]` | `months`, `cohorts`, `payers`, `per_payer_usd_micros` (null without a cohort that old) |
| `arppu` | `payers`, `net_usd_micros`, `arppu_usd_micros` |
| `by_asa_campaign[]` | `campaign_id`, `payers`, `payments`, `trials`, `refunds`, `gross_usd_micros`, `refunded_usd_micros`, `net_usd_micros` |
| `fx` | `as_of`, `missing`, `overrides` |
| `rebuilding` | `true` while the aggregates are being rebuilt from the ledger (the numbers may be incomplete) |

Rates are `null` when there is nothing to divide by.

Cohorts, Apple Ads and trial conversion read the purchase ledger at report time, only the purchases
the range needs. Everything else comes from the daily aggregates.

### Summary and overview

With `"summary": true` (same filters), the report is read from the daily aggregates without touching
the ledger. That makes it cheap enough for the revenue tree. It has only:

- `from`, `to`, `include_sandbox`, `totals`, `series`, `by_app`;
- `by_product`, each with the product's `kind` (empty for a deleted product);
- `subscriptions`, from the daily snapshots: no `active_now` / `mrr_usd_micros`;
- `fx` (`as_of`, `missing`);
- `paywall_views`: views of the placements of the selected apps' games in the range;
- `summary` and `rebuilding`.

`admin_revenue_overview(query_json)` is what the Overview and Apps pages show. It returns `from`, `to`,
`include_sandbox`, `totals`, `series`, `by_app`, `fx` and `rebuilding` of the last 30 days (shapes as
above).

### Kept reports

A report that took 300 ms or more to count is kept on the server for an hour. It is answered again to
the same question: the same kind, range, sandbox and builds the admin sees. Two admins who see the
same builds share it.

Nothing is counted in the background. A kept report is counted again only when someone asks:

- after an hour;
- on a new day;
- after a rebuild;
- with `"refresh": true`. The server honors it at most once a minute. Sooner, the kept one is answered.

New exchange rates, a rate override and a rebuild drop every kept report.

While one admin counts a kept report again, others get the previous one. While the first count of a
report runs, others get `[ERR_REPORT_COUNTING]`. The pages then ask again every few seconds instead of
counting it side by side.

Every answer of both procedures carries `computed_at_micros`, `refresh_after_micros`,
`stale_after_micros`, `kept` and `counting`. The pages show "updated … ago" with a refresh button that
waits for `refresh_after_micros`.

Reports of one game are fast and counted on every request.

### Report cost

A report's cost grows with the rows it reads (days × builds × products × currencies). SpacetimeDB
computes one call of a database at a time. While a report is counted, purchases and every other
request wait.

Measured on a local SpacetimeDB 2.16.1 with synthetic aggregates (about 25 daily rows per build on
average, a few big games):

| Builds | Rows per 30 days | Summary, 30 days | Summary, 90 days |
|---|---|---|---|
| 500 | ~220k | 0.8 s | 2.3 s |
| 1,000 | ~440k | 1.6 s | 5.2 s |
| 2,000 | ~880k | 4.0 s | 12.9 s |

The Overview's totals with 2,000 builds: 3.1 s when counted, 30 ms when kept.

Reports of one game read through its builds' indexes and take milliseconds. On a large server, open
server-wide reports over long ranges sparingly.

### Rebuilding the aggregates

`admin_rebuild_revenue(app_ids_json)` recomputes the aggregates from the ledger with the same rules.
It covers the builds in the JSON array (`""` or `[]`: every build).

- It only starts the rebuild. A scheduled step counts the ledger in batches: 2,500 purchases per
  transaction, one a second. That is about two minutes for 200k purchases.
- `rebuilding` is `true` until it is done.
- Builds outside the rebuild keep counting live.
- After importing a ledger, the rebuild happens automatically for the imported builds, once the
  imports have stopped for 5 minutes.

The Revenue page exports the daily series, subscriptions by month, countries, cohorts and Apple Ads
campaigns as CSV.
