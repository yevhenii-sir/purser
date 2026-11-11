import { STORE_SECRETS } from "./stores"

/** `game_app.platform`; see `lib/stores.ts` for names and stores. */
export type Platform = "android" | "ios" | "amazon" | "steam" | "web"
export type Kind = "consumable" | "non_consumable" | "subscription"
export type Delivery = "spacetimedb" | "webhook" | "none"

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
  args_format: string
  webhook_url: string
  allow_sandbox: boolean
  enabled: boolean
  client_consumes: boolean
  /** Store id; empty = the platform's default (`storeOf` in lib/stores.ts). */
  store: string
  /** App Store builds: how refund requests are answered (`off` / empty, `inform`, `prefer`). */
  apple_refund_mode: string
}

export interface Product {
  key: string
  app_id: string
  product_id: string
  kind: Kind
  default_price_micros: number
  currency: string
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
  status: string
  reason: string
  environment: string
  expires_at_ms: number
  price_micros: number
  currency: string
  attempts: number
  next_attempt_at_micros: number
  last_error: string
  created_at_micros: number
  updated_at_micros: number
  [k: string]: unknown
}

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
  expirations: number
  trials: number
  trial_conversions: number
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

export interface Stats {
  from: string
  to: string
  totals: Money & { refund_rate: number }
  series: (Money & { day: string })[]
  by_app: (Money & { app_id: string; commission_percent: number })[]
  by_product: (Money & { app_id: string; product_id: string })[]
  by_country: (Money & { country: string })[]
  proceeds: { estimated: boolean; percent: number; after_year_percent: number; by_app: Record<string, number> }
  cohorts: Cohort[]
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
    active_now: number
    mrr_usd_micros: number
    series: { day: string; active: number; mrr_usd_micros: number }[]
    new_subscriptions: number
    trials_started: number
    trial_conversions: number
    renewals: number
    expirations: number
    active_at_start: number | null
    /** Expirations per 30 days / active at the start of the range. */
    churn_rate: number | null
    /** Renewals / (renewals + expirations). */
    renewal_rate: number | null
    monthly: SubscriptionMonth[]
  }
  fx: { as_of: string | null; missing: string[]; overrides: Record<string, number> }
  /** The worker is recomputing the revenue aggregates: totals may be incomplete for a while. */
  rebuilding?: boolean
}

/** `admin_revenue_stats` with `{"summary": true}`: only the aggregates, no ledger scans. A server
 * without the summary mode answers with the full report, which has these fields too. */
export interface StatsSummary {
  from: string
  to: string
  totals: Partial<Money> & { net_usd_micros: number }
  series: (Partial<Money> & { day: string; net_usd_micros: number })[]
  by_app: (Partial<Money> & { app_id: string; net_usd_micros: number })[]
  rebuilding?: boolean
}

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
  total: number
  by_status: Record<string, number>
  /** Rows the worker still has to process, soonest retry first (at most 200). */
  pending: Pick<Purchase, "id" | "app_id" | "product_id" | "user_id" | "status" | "attempts" | "next_attempt_at_micros" | "last_error" | "updated_at_micros">[]
  pending_total: number
  apps: number
  products: number
  outbox: number
}

export const STATUSES = ["verified", "transfer_pending", "granted", "completed", "revoke_pending", "revoked", "expire_pending", "expired", "rejected"]
export const KINDS: Kind[] = ["consumable", "non_consumable", "subscription"]

/** Every secret name with its description: the stores' (from the registry), then the rest. */
export const SECRET_NAMES: [string, string][] = [
  ...STORE_SECRETS.map((s): [string, string] => [s.name, s.label]),
  ["spacetime_token", "Token of the identity the game database trusts"],
  ["webhook_secret", "HMAC key for webhook delivery"],
  ["appsflyer_s2s_key", "AppsFlyer S2S key"],
  ["ga4_api_secret", "GA4 Measurement Protocol API secret"],
  ["tenjin_sdk_key", "Tenjin SDK key"],
  ["integration_webhook_secret", "Signing key of the webhook integration"],
  ["adjust_s2s_token", "Adjust S2S security token"],
  ["appmetrica_post_api_key", "AppMetrica Post API key"],
  ["gcs_service_account_json", "Cloud Storage export: service account JSON key"],
  ["alert_telegram_bot_token", "Alerts: Telegram bot token (default scope)"],
  ["alert_slack_webhook_url", "Alerts: Slack incoming webhook URL (default scope)"],
  ["alert_discord_webhook_url", "Alerts: Discord webhook URL (default scope)"],
  ["alert_webhook_secret", "Alerts: signing key of the alert webhook (default scope)"],
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
