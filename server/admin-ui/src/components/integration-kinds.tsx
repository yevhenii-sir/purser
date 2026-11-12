import type { ComponentType, ReactNode } from "react"
import { ActivityIcon, ChartColumnIcon, CloudUploadIcon, CrosshairIcon, TargetIcon, TrendingUpIcon, WebhookIcon } from "lucide-react"

export interface Integration {
  key: string
  app_id: string
  kind: Kind
  enabled: boolean
  settings_json: string
}

export type Kind = "ga4" | "appsflyer" | "adjust" | "tenjin" | "appmetrica" | "webhook" | "gcs"
/** `settings_json` of an integration, as each kind's form edits it. */
export type Settings = Record<string, any>

interface KindDef {
  name: string
  icon: ComponentType<{ className?: string }>
  description: string
  secret: string
  secretLabel: string
  /** Default scope of the secret: per app (data stream, endpoint) or per developer account. */
  secretScope: "app" | "account"
  /** What the client must send in `attribution` with /verify and /restore. */
  client: ReactNode
  summary: (s: Settings) => string
}

export const KINDS: Record<Kind, KindDef> = {
  ga4: {
    name: "Google Analytics 4",
    icon: ChartColumnIcon,
    description: "Purchases, renewals and refunds as GA4 / Firebase events through the Measurement Protocol.",
    secret: "ga4_api_secret",
    secretLabel: "Measurement Protocol API secret",
    secretScope: "app",
    client: <>Optional: the Firebase <code>app_instance_id</code> (otherwise one is derived from the user id).</>,
    summary: (s) => s.firebase_app_id || "",
  },
  appsflyer: {
    name: "AppsFlyer",
    icon: CrosshairIcon,
    description: "In-app events through the S2S Events API, attributed to the install.",
    secret: "appsflyer_s2s_key",
    secretLabel: "S2S key (Settings → API tokens)",
    secretScope: "account",
    client: <>Required: <code>appsflyer_id</code>. Recommended: <code>advertising_id</code> / <code>idfa</code>, <code>idfv</code>, <code>att</code>.</>,
    summary: (s) => s.app_id || "",
  },
  adjust: {
    name: "Adjust",
    icon: TargetIcon,
    description: "Purchases, renewals and refunds as Adjust S2S events with revenue, attributed to the install.",
    secret: "adjust_s2s_token",
    secretLabel: "S2S security token (app → Secure → S2S Security)",
    secretScope: "app",
    client: <>Required: <code>adjust_id</code> (Adjust.getAdid()) or a device id: <code>advertising_id</code> on Android, <code>idfa</code> / <code>idfv</code> on iOS.</>,
    summary: (s) => s.app_token || "",
  },
  tenjin: {
    name: "Tenjin",
    icon: TrendingUpIcon,
    description: "Purchases and refunds through the Tenjin S2S Purchase API for ROAS and LTV.",
    secret: "tenjin_sdk_key",
    secretLabel: "SDK key (Configure → Apps)",
    secretScope: "account",
    client: <>Required: <code>tenjin_id</code> (Tenjin analytics_installation_id) and <code>os_version</code>. Recommended: <code>advertising_id</code> / <code>idfa</code>, <code>idfv</code>, <code>app_version</code>.</>,
    summary: (s) => [s.send_expire && "expirations", s.send_sandbox && "sandbox"].filter(Boolean).join(", "),
  },
  appmetrica: {
    name: "AppMetrica",
    icon: ActivityIcon,
    description: "Purchases, renewals and refunds as revenue events through the AppMetrica Post API.",
    secret: "appmetrica_post_api_key",
    secretLabel: "Post API key (app settings → Post API)",
    secretScope: "app",
    client: <>Required: <code>appmetrica_device_id</code> (AppMetrica.RequestAppMetricaDeviceID), unless the game sets the AppMetrica profile id to the billing user id. Recommended: <code>advertising_id</code> / <code>idfa</code>, <code>idfv</code>, <code>os_version</code>, <code>app_version</code>. AppMetrica only takes ids its SDK has already sent.</>,
    summary: (s) => s.application_id || "",
  },
  webhook: {
    name: "Webhook",
    icon: WebhookIcon,
    description: "Every purchase event as signed JSON to your own endpoint: analytics, a data warehouse, chat notifications.",
    secret: "integration_webhook_secret",
    secretLabel: "Signing key (X-Billing-Signature)",
    secretScope: "app",
    client: <>Nothing: the event carries whatever the client sent.</>,
    summary: (s) => s.url || "",
  },
  gcs: {
    name: "Google Cloud Storage",
    icon: CloudUploadIcon,
    description: "Every event as a JSON file in your bucket, one folder per day: load it into BigQuery or another data warehouse for your own reports.",
    secret: "gcs_service_account_json",
    secretLabel: "Service account JSON key (Storage Object Creator on the bucket)",
    secretScope: "app",
    client: <>Nothing: the event carries whatever the client sent.</>,
    summary: (s) => (s.bucket ? `gs://${s.bucket}/${s.folder || ""}` : ""),
  },
}
export const KIND_LIST = Object.keys(KINDS) as Kind[]
