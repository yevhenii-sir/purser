import { STORE_SECRETS } from "./stores"

/** `game_app.platform`; see `lib/stores.ts` for names and stores. */
export type Platform = "android" | "ios" | "amazon" | "steam" | "web"
export type Kind = "consumable" | "non_consumable" | "subscription"
export type Delivery = "spacetimedb" | "spacetimedb_idc" | "webhook" | "none"

export interface GameApp {
  app_id: string
  platform: Platform
  game_key: string
  account: string
  delivery: Delivery
  spacetime_url: string
  spacetime_db: string
  grant_reducer: string
  revoke_reducer: string
  webhook_url: string
  allow_sandbox: boolean
  enabled: boolean
  client_consumes: boolean
  /** Store id, resolved by the server (`storeOf` in lib/stores.ts). */
  store: string
  /** App Store builds: how refund requests are answered (`off` / empty, `inform`, `prefer`). */
  apple_refund_mode: string
  /** `open` | `signed`: whether client requests need a player token signed by the game's backend. */
  client_auth: string
  /** Amazon builds: the only SNS topic notifications are accepted from (empty: any). */
  sns_topic_arn: string
}

export interface Product {
  key: string
  app_id: string
  product_id: string
  kind: Kind
  price_micros: number
  currency: string
  /** Store listing used by Create in store. */
  title: string
  description: string
  /** Subscriptions: `P1M` etc. (`lib/periods.ts`), "" if unknown. */
  period: string
  apple_group: string
  /** The app's own id of the product, the same in every store; empty = `product_id` (`catalogIdOf`). */
  catalog_id: string
  /** Access levels the product unlocks, comma-separated (`premium,no_ads`). */
  access: string
}

/** The app's own id of a product (paywalls and entitlements use it): its catalog id, else its store id. */
export const catalogIdOf = (p: Pick<Product, "product_id" | "catalog_id">) => p.catalog_id || p.product_id

/** The store listing fields of a product as `admin_upsert_products` takes them (only the ones that are set). */
export function listingOf(p: Partial<Pick<Product, "title" | "description" | "period" | "apple_group">> | undefined) {
  const out: Record<string, string> = {}
  for (const k of ["title", "description", "period", "apple_group"] as const) if (p?.[k]) out[k] = p[k]!
  return out
}

export interface Purchase {
  id: string
  store: string
  app_id: string
  product_id: string
  kind: Kind
  transaction_id: string
  original_transaction_id: string
  purchase_token: string
  user_id: string
  status: PurchaseStatus
  reason: string
  environment: string
  expires_at_ms: number
  price_micros: number
  currency: string
  /** Units bought at once; `price_micros` is what all of them cost. */
  quantity: number
  /** Refunded while the purchase stays (partial refunds). */
  refunded_micros: number
  /** Tax in `price_micros` when the store reported it (Google Orders API); 0: unknown. */
  tax_micros: number
  /** A prorated store refund: the part it returned, in thousandths of a percent; 0: all of it. */
  refund_share_milli: number
  /** Subscriptions: a price change of the next renewals (`consent_needed`, `accepted`, `scheduled`); empty: none. */
  price_change: string
  /** Its new price (in `currency`); 0 when unknown. */
  price_change_micros: number
  attempts: number
  next_attempt_at_micros: number
  last_error: string
  created_at_micros: number
  updated_at_micros: number
  [k: string]: unknown
}

/** Why a queued purchase waits (`purser_core::model::waiting`). */
export type Waiting = "running" | "blocked" | "game_answer" | "due" | "retry" | "later"

export type Level = "skip" | "info" | "ok" | "warn" | "fail"

export interface Check {
  id: string
  title: string
  level: Level
  detail: string
  fix: string
}

export interface Report {
  app_id: string
  checked_at_ms: number
  summary: Level
  counts?: { ok: number; warn: number; fail: number }
  checks: Check[]
  urls: { base?: string; store?: string; notify?: string; notify_label?: string; billing_identity?: string }
}

export interface CatalogRow {
  product_id: string
  kind: Kind
  price_micros: number
  currency: string
  billing_period_days: number
  /** Subscription period (`P1M`) when the source names it directly (a Unity config). */
  period?: string
  status: "new" | "changed" | "same"
  local_kind: string
}

export interface Money {
  gross_usd_micros: number
  refunded_usd_micros: number
  net_usd_micros: number
  purchases: number
  new_subscriptions: number
  renewals: number
  refunds: number
  transfers: number
  /** Paid subscriptions that ended (free trials that ended unpaid are `trial_expirations`). */
  expirations: number
  trials: number
  /** First payments after a free trial (also in `new_subscriptions`). */
  trial_conversions: number
  trial_expirations: number
  /** Estimate: commission from the store fee settings. */
  store_fee_usd_micros: number
  proceeds_usd_micros: number
}

export interface SubscriptionMonth {
  month: string
  active_start: number | null
  new_subscriptions: number
  trials: number
  trial_conversions: number
  trial_expirations: number
  /** Trials that ended paid ÷ all trials that ended. */
  trial_conversion_rate: number | null
  renewals: number
  expirations: number
  churn_rate: number | null
  renewal_rate: number | null
}

export interface Cohort {
  cohort: string
  payers: number
  /** Cumulative net revenue of the cohort by month since the first payment (index 0 = that month). */
  revenue_usd_micros: number[]
  per_payer_usd_micros: number[]
}

export interface AsaCampaign {
  campaign_id: string
  payers: number
  payments: number
  trials: number
  refunds: number
  gross_usd_micros: number
  refunded_usd_micros: number
  net_usd_micros: number
}

/** When a revenue report was counted. Slow ones are kept on the server for 10 minutes (times in µs). */
export interface Kept {
  computed_at_micros: number
  /** A refresh before this time gets the kept report. */
  refresh_after_micros: number
  stale_after_micros: number
  /** Answered from the kept reports (it was slow to count). */
  kept: boolean
  /** Another admin is counting it again now. */
  counting: boolean
}

export interface Stats extends Partial<Kept> {
  /** How much analytics the server keeps (`analytics` setting): `light` leaves the ledger part empty. */
  analytics?: "full" | "light" | "off"
  from: string
  to: string
  totals: Money & { refund_rate: number }
  series: (Money & { day: string })[]
  by_app: (Money & { app_id: string; commission_percent: number })[]
  by_product: (Money & { app_id: string; product_id: string })[]
  by_country: (Money & { country: string })[]
  proceeds: { estimated: boolean; percent: number; after_year_percent: number; by_app: Record<string, number> }
  cohorts: Cohort[]
  /** Net revenue per payer after 1, 3, 6 and 12 months, over the cohorts old enough (null: none yet). */
  ltv: { months: number; cohorts: number; payers: number; per_payer_usd_micros: number | null }[]
  /** Net revenue of the period's paid purchases per paying user. */
  arppu: { payers: number; net_usd_micros: number; arppu_usd_micros: number | null }
  by_asa_campaign: AsaCampaign[]
  trials: { started: number; converted: number; pending: number; conversion_rate: number | null }
  by_currency: {
    currency: string
    gross_micros: number
    refunded_micros: number
    gross_usd_micros: number
    refunded_usd_micros: number
    converted: boolean
  }[]
  subscriptions: {
    /** Paid subscriptions running now; free trials are `trials_now`. */
    active_now: number
    trials_now: number
    mrr_usd_micros: number
    series: { day: string; active: number; trials: number; mrr_usd_micros: number }[]
    /** Paid subscriptions started: paid at once or after a trial (`trial_conversions`). */
    new_subscriptions: number
    trials_started: number
    trial_conversions: number
    trial_expirations: number
    trial_conversion_rate: number | null
    renewals: number
    expirations: number
    active_at_start: number | null
    /** Expirations per 30 days / active at the start of the range. */
    churn_rate: number | null
    /** Renewals / (renewals + expirations). */
    renewal_rate: number | null
    monthly: SubscriptionMonth[]
    /** Subscriptions giving access now that will not renew / are in a grace period / have a billing issue. */
    auto_renew_off_now: number
    in_grace_now: number
    billing_issue_now: number
    /** A price increase the player has not accepted: these end unless they do. */
    price_consent_needed_now?: number
  }
  fx: { as_of: string | null; missing: string[]; overrides: Record<string, number> }
  /** The worker is recomputing the revenue aggregates: totals may be incomplete for a while. */
  rebuilding?: boolean
}

/** `admin_revenue_stats` with `{"summary": true}`: only the aggregates, no ledger scans. */
export interface StatsSummary {
  from: string
  to: string
  totals: Money
  series: (Money & { day: string })[]
  by_app: (Money & { app_id: string })[]
  rebuilding?: boolean
}

/** `admin_revenue_overview`: the summary's totals, series and per-app numbers. */
export interface RevenueOverview extends StatsSummary, Kept {}

/** `integration_health`: what happened to the events of one integration of a build (times in µs, 0 = never). */
export interface IntegrationHealth {
  key: string
  app_id: string
  kind: string
  last_sent_at_micros: number
  last_failed_at_micros: number
  last_error: string
  /** The last failure dropped the event (not retried). */
  last_failure_final: boolean
  /** Last event skipped because the client did not send the device id the integration needs. */
  last_skipped_at_micros: number
  last_skip_reason: string
}

/** `admin_overview`: ledger counters for the Overview page. */
export interface Overview {
  /** Seconds since the ledger worker last ran (null before its first run). */
  worker_age_secs: number | null
  /** Older than this: the worker has stopped (`/healthz` answers 503). */
  worker_stale_after_secs: number
  total: number
  by_status: Record<string, number>
  /** Rows the worker still has to process, soonest retry first (at most 200), with what each waits for. */
  pending: (Pick<Purchase, "id" | "app_id" | "product_id" | "user_id" | "status" | "attempts" | "next_attempt_at_micros" | "last_error" | "updated_at_micros"> & {
    waiting: Waiting
  })[]
  pending_total: number
  /** Rows due now (the rest wait for a later retry) and when the longest waiting one became due (0: none). */
  due_total: number
  oldest_due_micros: number
  /** Due this long: the queue is behind (`/healthz` says `backlog`). */
  backlog_after_secs: number
  /** Queue drainers running or scheduled, and how many may run at once (`worker_lanes`). */
  drainers: number
  lanes: number
  /** Apps (`game_key` groups) and their builds. */
  apps: number
  builds: number
  products: number
  outbox: number
}

/** Ledger states (`purser_core::model::status`). */
export const STATUSES = ["verified", "transfer_pending", "granted", "completed", "revoke_pending", "revoked", "expire_pending", "expired", "rejected"] as const
export type PurchaseStatus = (typeof STATUSES)[number]
export const KINDS: Kind[] = ["consumable", "non_consumable", "subscription"]

/** Every secret name with its description: the stores' (from the registry), then the rest. */
export const SECRET_NAMES: [string, string][] = [
  ...STORE_SECRETS.map((s): [string, string] => [s.name, s.label]),
  ["spacetime_token", "Token of the identity the game database trusts"],
  ["webhook_secret", "HMAC key for webhook delivery"],
  ["client_auth_secret", "Key the game's backend signs player tokens with (Client requests: signed)"],
  ["response_signing_key", "Ed25519 key the server signs /entitlements and /restore answers with"],
  ["appsflyer_s2s_key", "AppsFlyer S2S key"],
  ["ga4_api_secret", "GA4 Measurement Protocol API secret"],
  ["tenjin_sdk_key", "Tenjin SDK key"],
  ["integration_webhook_secret", "Signing key of the webhook integration"],
  ["adjust_s2s_token", "Adjust S2S security token"],
  ["appmetrica_post_api_key", "AppMetrica Post API key"],
  ["gcs_service_account_json", "Cloud Storage export: service account JSON key"],
  ["amplitude_api_key", "Amplitude project API key"],
  ["mixpanel_service_account", "Mixpanel service account (username:secret)"],
  ["meta_access_token", "Meta Conversions API access token"],
  ["alert_telegram_bot_token", "Alerts: Telegram bot token (scope alert-<channel id>; set it on the Alerts page)"],
  ["alert_slack_webhook_url", "Alerts: Slack incoming webhook URL (scope alert-<channel id>; set it on the Alerts page)"],
  ["alert_discord_webhook_url", "Alerts: Discord webhook URL (scope alert-<channel id>; set it on the Alerts page)"],
  ["alert_webhook_secret", "Alerts: signing key of the alert webhook (scope alert-<channel id>; set it on the Alerts page)"],
  ["metrics_token", "Bearer token of GET /metrics (scope default; create it on the Server page)"],
]

/** A product being created in a build's store (`store_create_job`). */
export interface StoreCreateJob {
  id: number
  app_id: string
  store: string
  product_id: string
  kind: Kind
  input_json: string
  state_json: string
  status: "pending" | "done" | "failed"
  step: string
  error: string
  note: string
  store_product_id: string
  retries: number
  created_at_micros: number
  updated_at_micros: number
}
