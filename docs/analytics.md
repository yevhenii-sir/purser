# Revenue analytics

The admin panel's **Revenue** page shows what the stores paid, in USD: net revenue, estimated proceeds, refunds, purchases, subscriptions (new, trials, trial conversion, renewals, churn), active subscriptions and MRR, a daily chart and breakdowns by app, product, currency, billing country and Apple Ads campaign, plus payer cohorts. It works from the purchase ledger, so no analytics SDK is needed. The same page is on every app's **Revenue** tab, limited to that app's builds.

## What is counted

| Metric | When | Day |
|---|---|---|
| Gross revenue, purchases, new subscriptions, renewals | the first time a store purchase is granted | the day the purchase was verified |
| Free trials | the first grant of a free-trial period (no money) | the day the trial started |
| Trial conversions | the first paid period of a subscription after a free trial (also a renewal) | the day it was verified |
| Refunds | the store reports a refund of a granted purchase | the day it is reported |
| Expirations, transfers | a subscription expires / an owned product moves to another user | the day it happens |
| Active subscriptions, MRR | subscriptions paid until a future date, renewals counted once | snapshot taken daily |

- Manual grants are not revenue. Sandbox purchases are excluded unless *Include sandbox* is on.
- Amounts are store prices before the store's commission and taxes: the App Store reports the price; Google reports the price of subscriptions; for Google one-time products the price comes from the client (`price_micros` in `/verify`) or the product's default price. Set default prices on Android products (the setup check warns about missing ones).
- A free-trial period is recorded with price 0 (Google reports the price of the next renewal, which is not what the buyer paid), so integrations get no revenue for it either. Trials are detected from Google's `lineItems[].offerPhase.freeTrial` (subscriptionsv2) and Apple's `offerDiscountType` = `FREE_TRIAL` (older Apple payloads: introductory offer with price 0).
- MRR divides each subscription's price by its billing period, guessed from the paid period (weekly, monthly, quarterly, half-yearly, yearly).
- Days are UTC.

## Billing country

Every purchase keeps the buyer's billing country (ISO 3166-1 alpha-2): Google's `regionCode` (one-time products and subscriptions), Apple's `storefront` (alpha-3, converted). Purchases recorded before this was added show as *Unknown*. **By country** lists gross, refunds, net, estimated proceeds, payments and trials per country.

## Proceeds (estimate)

Proceeds = net revenue − estimated store fee. The fee is a commission percent applied at report time (changing it needs no rebuild):

- **Default commission**: 15% (Google Play's first $1M a year and the App Store Small Business Program), or 30% (standard rate).
- **Per build**: a different percent for some apps (e.g. 30% for a big title, 15% for the Android build because Google Play takes 15% on all subscriptions).
- **Subscriptions after 12 paid months**: both stores charge 15% on a subscription after its first year of paid service. A renewal recorded 365 days or more after the subscription's first paid period uses `min(app percent, this percent)` (default 15).
- Refunds return the commission too: the fee is taken on net revenue.

Taxes (VAT, digital services taxes) are not modelled, so real payouts are lower in countries where the store withholds them. Settings: Revenue → **Store fee**, or `admin_set_setting` with the server settings `store_commission_percent` (`"30"`), `store_commission_after_year_percent` (`"15"`) and `store_commission_by_app` (`{"com.example.mygame": 30}`); an empty value restores the default.

## Subscriptions

- **New subscriptions** = paid starts (no trial) + trial conversions. **Trials started** counts free trials.
- **Trial → paid**: of the free trials started in the period, the share that has a later paid period of the same subscription. Trials still running (not expired, not paid yet) are left out of the rate.
- **Renewal rate** = renewals ÷ (renewals + expirations): of the subscription periods that ended, the share that continued. Renewals include the first paid period after a trial; expirations include trials that ended unpaid.
- **Churn** = expirations ÷ active subscriptions at the first daily snapshot of the period (or month), per 30 days. The snapshots start on the day the analytics version was installed, so churn is empty before that.
- **Subscriptions by month** shows these per calendar month. Auto-renew turned off is not counted separately: the subscription counts once it expires.

## Revenue tree

The **Revenue tree** on the Revenue page (and on an app's Revenue tab) shows where net revenue comes from, left to right: paywall views and trials started, then purchases, new subscriptions, renewals and expirations, then one-time sales and subscription revenue, gross revenue and refunds, and net revenue. Every number has its change against the previous period of the same length (green when it moved the right way: down for refunds and expirations). The labels on the links are ratios:

- paywall views → purchases / new subscriptions: payments per view (hidden when there are more payments than views, i.e. most payments happen outside paywalls);
- trials → new subscriptions: trial conversions ÷ trials started in the period (conversions of trials started earlier count too, so it is a rough rate; **Trial → paid** above is the exact one);
- purchases → one-time sales: the average one-time purchase;
- renewals → subscriptions: renewals ÷ (renewals + expirations), here without the first paid period after a trial;
- expirations → active subscriptions: churn per 30 days;
- one-time sales / subscriptions → gross: their share; refunds → net: refunds ÷ gross.

Active subscriptions and MRR are those of the last daily snapshot of the period. Subscription nodes are hidden when the apps have no subscriptions, the paywall node when no views were reported. Paywall views are not split by environment (sandbox views count). The tree reads two summary reports (the period and the one before), so it is as cheap as the Overview.

## Payer cohorts

Payers (user ids with a granted, non-trial store purchase) are grouped by the month of their first payment, for cohorts that start in the selected period. Each cell is the cohort's cumulative net revenue per payer by month since the first payment (M0 = that month), up to today; refunded purchases count as 0. Computed from the ledger when the report is opened.

## Apple Ads

When the Unity package resolves the install's Apple Ads attribution (`asa_campaign_id`, kept per user in `user_attribution`), **By Apple Ads campaign** shows payers, payments, trials, gross, refunds and net per campaign for purchases made in the period. Only campaign ids are known (no names). Computed from the ledger when the report is opened.

## Currencies

Every amount is kept in its own currency and converted to USD with the European Central Bank reference rate of that day ([frankfurter.dev](https://frankfurter.dev), no key needed; weekends use Friday's rate). The worker fetches the latest rates every 6 hours and backfills history as far back as the first sale.

The ECB publishes about 30 currencies. Others (UAH, VND, …) are listed as *No rate for …* and left out of USD totals until you set a manual rate: Revenue → By currency → **Set rate** (`admin_set_fx_override("UAH", 41.5)`: units per 1 USD). Totals per currency are always shown in the original currency.

## Data and API

- `revenue_daily`: one row per day × app × product × environment × currency, updated in the same transaction as the ledger. Besides the counts and amounts it keeps trials, trial conversions and the gross / refunds of subscription periods after the first paid year.
- `revenue_country_daily`: the same per day × app × billing country × environment × currency.
- `subscription_daily`: active subscriptions per day.
- `fx_day`, `fx_override`: exchange rates.
- `purchase.country`, `purchase.trial`: billing country and free-trial flag of each ledger row.

`admin_revenue_stats(query_json)` returns the report as JSON: `{"from": "2026-10-01", "to": "2026-10-31", "game_key": "my-game", "app_ids": [], "include_sandbox": false}`, all fields optional (default: the last 30 days, all apps). Main fields:

| Field | Content |
|---|---|
| `totals`, `series[]`, `by_app[]`, `by_product[]`, `by_country[]` | `gross_usd_micros`, `refunded_usd_micros`, `net_usd_micros`, `store_fee_usd_micros`, `proceeds_usd_micros`, `purchases`, `new_subscriptions`, `renewals`, `refunds`, `transfers`, `expirations`, `trials`, `trial_conversions` (`by_app` also `commission_percent`) |
| `by_currency[]` | amounts in the original currency and in USD, `converted` |
| `proceeds` | the commission used: `percent`, `after_year_percent`, `by_app`, `estimated: true` |
| `subscriptions` | `active_now`, `mrr_usd_micros`, `series[]`, `new_subscriptions`, `trials_started`, `trial_conversions`, `renewals`, `expirations`, `active_at_start`, `churn_rate`, `renewal_rate`, `monthly[]` |
| `trials` | `started`, `converted`, `pending`, `conversion_rate` (trials started in the period) |
| `cohorts[]` | `cohort` (`YYYY-MM`), `payers`, `revenue_usd_micros[]` and `per_payer_usd_micros[]` (cumulative by month since the first payment) |
| `by_asa_campaign[]` | `campaign_id`, `payers`, `payments`, `trials`, `refunds`, `gross_usd_micros`, `refunded_usd_micros`, `net_usd_micros` |
| `fx` | `as_of`, `missing`, `overrides` |
| `rebuilding` | `true` while the aggregates are being rebuilt from the ledger (the numbers may be incomplete) |

Rates are `null` when there is nothing to divide by. Cohorts, Apple Ads and trial conversion read the purchase ledger at report time (only the purchases the range needs); everything else comes from the daily aggregates.

With `"summary": true` (same filters) the report has only `from`, `to`, `include_sandbox`, `totals`, `series`, `by_app`, `by_product` (each with the product's `kind`, empty for a deleted product), `subscriptions` (from the daily snapshots: no `active_now` / `mrr_usd_micros`), `fx` (`as_of`, `missing`), `paywall_views` (views of the placements of the selected apps' games in the range), `summary` and `rebuilding`, read from the daily aggregates without touching the ledger: cheap enough for overview pages and the revenue tree.

`admin_rebuild_revenue()` recomputes the aggregates from the ledger with the same rules. It only starts the rebuild: a scheduled step counts the ledger in batches (2,500 purchases per transaction, one a second: about two minutes for 200k purchases) and `rebuilding` is `true` until it is done. After importing a ledger it happens automatically, once the imports have stopped for 5 minutes. After an upgrade from a version without analytics, or without the per-country aggregates, the worker rebuilds the history the same way.

The Revenue page exports the daily series, subscriptions by month, countries, cohorts and Apple Ads campaigns as CSV.
