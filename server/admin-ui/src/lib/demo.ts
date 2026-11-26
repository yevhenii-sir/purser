// Demo build of the admin page (`npm run build:demo`): no server, no sign-in. Reads answer from a
// snapshot of a database filled with made-up apps and purchases (`demo/snapshot.json`, made by
// tools/demo_snapshot.py); every change is refused. The clock is set back to the moment the
// snapshot was taken, so "last 30 days" and "2 minutes ago" mean what they meant then.

import snapshot from "../../demo/snapshot.json"
import { lookups, project, type Query } from "./api"
import { storeOf } from "./stores"
import type { GameApp } from "./types"

/** Rows of the snapshot (made by tools/demo_snapshot.py from real tables): loosely typed. */
type Row = Record<string, any>

interface Snapshot {
  generated_at_ms: number
  version: string
  identity: string
  tables: Record<string, Row[]>
  procs: {
    admin_overview: unknown
    revenue: Record<string, unknown>
    profiles: Record<string, unknown>
    paywall_results: Record<string, unknown>
    previews: Record<string, unknown>
    checks: Record<string, unknown>
  }
}

const S = snapshot as unknown as Snapshot
const DAY = 86_400_000

export const READ_ONLY = "This is a read-only demo with sample data: changes are not saved."

/** Sets the page clock back to when the snapshot was taken (call before anything reads the time). */
export function installClock() {
  const RealDate = Date
  const offset = RealDate.now() - S.generated_at_ms
  class DemoDate extends RealDate {
    constructor(...args: ConstructorParameters<DateConstructor> | []) {
      if (args.length === 0) super(RealDate.now() - offset)
      else super(...(args as ConstructorParameters<DateConstructor>))
    }
    static now() {
      return RealDate.now() - offset
    }
  }
  globalThis.Date = DemoDate as DateConstructor
}

// --- the analytics mode ----------------------------------------------------------------------------

/** The analytics mode a visitor picked on the Server page: this tab only (nothing is saved), so the
 * demo can show what each mode leaves on the revenue pages. */
const MODE_KEY = "purser-demo-analytics"
let analyticsMode: string = (() => {
  try {
    return sessionStorage.getItem(MODE_KEY) || "full"
  } catch {
    return "full"
  }
})()

function setAnalytics(key: string, value: string) {
  if (key !== "analytics") throw new Error(READ_ONLY)
  const mode = value.trim().toLowerCase() || "full"
  if (!["full", "light", "off"].includes(mode)) throw new Error(`[ERR_BAD_SETTING] analytics must be full, light or off`)
  analyticsMode = mode
  try {
    sessionStorage.setItem(MODE_KEY, mode)
  } catch {
    // Private mode: the choice lasts until the page is reloaded.
  }
  return null
}

const ANALYTICS_OFF = "[ERR_ANALYTICS_OFF] revenue analytics are off on this server (Server page: Analytics)"

/** A full report as the server answers it in `light` mode: nothing read from the ledger. */
function light(r: Row): Row {
  if (!r.cohorts) return { ...r, analytics: "light" }
  return {
    ...r,
    analytics: "light",
    cohorts: [],
    install_cohorts: (r.install_cohorts || []).map((c: Row) => ({ ...c, payers: 0, payer_rate: 0, revenue_usd_micros: [], per_install_usd_micros: [] })),
    ltv: (r.ltv || []).map((l: Row) => ({ ...l, cohorts: 0, payers: 0, per_payer_usd_micros: null })),
    arppu: { payers: 0, net_usd_micros: 0, arppu_usd_micros: null },
    by_asa_campaign: [],
    trials: { started: 0, converted: 0, pending: 0, conversion_rate: null },
    subscriptions: { ...r.subscriptions, active_now: 0, trials_now: 0, mrr_usd_micros: 0, auto_renew_off_now: 0, in_grace_now: 0, billing_issue_now: 0 },
  }
}

// --- reads ---------------------------------------------------------------------------------------

/** A table read (`select`), answered from the snapshot the way `admin_select` filters. */
export function select(q: Query): Row[] {
  let rows = S.tables[q.table] || []
  if (q.table === "server_state") rows = [...rows.filter((r: Row) => r.key !== "analytics"), { key: "analytics", value: analyticsMode }]
  const hits = lookups(q).flatMap(([col, v]) => (col ? rows.filter((r) => String(r[col]) === v) : rows))
  return project(hits.map((r) => ({ ...r })), q.columns)
}

/** Raw SQL only writes here (claiming the database): refused. */
export function sql(): never {
  throw new Error(READ_ONLY)
}

// --- /call ---------------------------------------------------------------------------------------

const isoDay = (ms: number) => new Date(ms).toISOString().slice(0, 10)

function revenue(json: string) {
  if (analyticsMode === "off") throw new Error(ANALYTICS_OFF)
  const r = recordedRevenue(json) as Row
  return analyticsMode === "light" ? light(r) : r
}

function recordedRevenue(json: string) {
  const q = JSON.parse(json || "{}")
  const to = q.to || isoDay(Date.now())
  const from = q.from || isoDay(Date.parse(to) - 29 * DAY)
  const key = [from, to, q.game_key || "", q.summary ? "s" : "f"].join("|")
  const hit = S.procs.revenue[key]
  if (hit) return hit
  // Another range: the closest recorded one of the same game and kind.
  const want = (Date.parse(to) - Date.parse(from)) / DAY
  let best: [number, unknown] | null = null
  for (const [k, v] of Object.entries(S.procs.revenue)) {
    const [f, t, g, s] = k.split("|")
    if (g !== (q.game_key || "") || s !== (q.summary ? "s" : "f") || t !== isoDay(Date.now())) continue
    const d = Math.abs((Date.parse(t) - Date.parse(f)) / DAY - want)
    if (!best || d < best[0]) best = [d, v]
  }
  if (best) return best[1]
  throw new Error("This range is not in the demo data.")
}

function listPurchases(json: string) {
  const q = JSON.parse(json || "{}")
  const apps: string[] = q.app_ids || []
  const rows = (S.tables.purchase || [])
    .filter((p) => Number(p.created_at_micros) >= Number(q.since_micros || 0))
    .filter((p) => !apps.length || apps.includes(p.app_id))
    .filter((p) => !q.status || p.status === q.status)
    .filter((p) => !q.kind || p.kind === q.kind)
    .filter((p) => !q.subscription || (p.kind === "subscription" && ["verified", "granted", "completed"].includes(p.status) && Number(p.expires_at_ms) > Date.now()))
    .sort((a, b) => Number(b.created_at_micros) - Number(a.created_at_micros))
  const limit = Number(q.limit) || 200
  // The demo pages by position: `before_id` is the index to continue from.
  const start = Number(q.before_id || 0)
  const page = rows.slice(start, start + limit)
  const more = rows.length > start + limit
  return { rows: page, more, next: more ? { until_micros: 0, before_id: String(start + limit) } : null }
}

function findPlayer(query: string) {
  const q = query.trim()
  const matches: { user_id: string; via: string; purchase_id: string }[] = []
  for (const p of S.tables.purchase || []) {
    const via =
      p.user_id === q ? "user_id"
      : p.id === q || p.transaction_id === q ? "transaction"
      : p.original_transaction_id === q ? "original_transaction"
      : p.purchase_token === q ? "purchase_token"
      : ""
    if (via && !matches.some((m) => m.user_id === p.user_id)) matches.push({ user_id: p.user_id, via, purchase_id: p.id })
  }
  return { matches }
}

/** What Create in store would do with the sample builds: products the build lists are already in the store. */
function storeCreatePreview(json: string) {
  const q = JSON.parse(json || "{}") as { app_ids: string[]; products: { product_id: string; kind: string }[] }
  const rows = (S.tables.game_app || []).filter((a: Row) => q.app_ids.includes(a.app_id)).flatMap((a: Row) => {
    const store = storeOf(a as GameApp)
    return q.products.map((p) => {
      const has = (S.tables.product || []).find((x: Row) => x.app_id === a.app_id && (x.catalog_id || x.product_id) === p.product_id)
      const status = !store.product_create.includes(p.kind)
        ? "unsupported"
        : has ? "exists" : "create"
      const reason = status !== "unsupported" ? ""
        : store.product_create.length ? `${store.name} cannot create ${p.kind.replace("_", "-")} products here`
        : `${store.name} products are created in the store's own console`
      return { app_id: a.app_id, store: store.id, product_id: p.product_id, kind: p.kind, status, reason, store_product_id: has ? String(has.product_id) : "" }
    })
  })
  const apple = (S.tables.game_app || []).filter((a: Row) => q.app_ids.includes(a.app_id) && storeOf(a as GameApp).id === "apple")
  return { rows, apple_groups: Object.fromEntries(apple.map((a: Row) => [a.app_id, ["Premium"]])), warnings: {}, pending: [] }
}

const PROCS: Record<string, (...args: never[]) => unknown> = {
  admin_store_create_preview: storeCreatePreview,
  admin_set_setting: setAnalytics,
  whoami: () => "0x" + S.identity,
  admin_response_public_key: () => ({ public_key: null }),
  admin_transfer_policy: () => ({ mode: "always", cooldown_days: 0, max_transfers: 0 }),
  // `?role=viewer` (editor, support, admin) shows the demo as that role sees it.
  admin_me: () => {
    const asked = new URLSearchParams(location.search).get("role") || ""
    const role = ["admin", "editor", "support", "viewer"].includes(asked) ? asked : "owner"
    return { identity: S.identity, name: S.tables.admin?.find((a: Row) => a.protected)?.name || "", role, owner: role === "owner", game_keys: [] }
  },
  admin_overview: () => S.procs.admin_overview,
  admin_select: (json: string) => {
    const q = JSON.parse(json)
    if (q.table === "admin_audit" && q.where?.column === "since") {
      return (S.tables.admin_audit || []).filter((r: Row) => Number(r.at_micros) >= Number(q.where.equals))
    }
    return select({ table: q.table, columns: q.columns, where: q.where })
  },
  admin_apps_summary: () => (S.tables.game_app || []).map((a: Row) => {
    const check = (S.tables.app_check || []).find((c: Row) => c.app_id === a.app_id)
    return {
      app_id: a.app_id, summary: check?.summary || "", checked_at_micros: check?.checked_at_micros || 0,
      catalog_ids: (S.tables.product || []).filter((p: Row) => p.app_id === a.app_id).map((p: Row) => p.catalog_id || p.product_id),
      integrations: (S.tables.integration || []).filter((i: Row) => i.app_id === a.app_id && i.enabled).map((i: Row) => i.kind),
    }
  }),
  admin_revenue_stats: revenue,
  admin_revenue_overview: () => {
    const now = Date.now() * 1000
    return { ...(revenue(JSON.stringify({ summary: true })) as object), computed_at_micros: now, refresh_after_micros: now + 60e6, stale_after_micros: now + 600e6, counting: false }
  },
  admin_list_purchases: listPurchases,
  admin_find_player: findPlayer,
  admin_user_profile: (user: string) => {
    const p = S.procs.profiles[user]
    if (!p) throw new Error("This player has no purchases in the demo data.")
    return p
  },
  admin_check_app: (app: string) => S.procs.checks[app] ?? { error: READ_ONLY },
  admin_paywall_results: (json: string) => {
    const q = JSON.parse(json || "{}")
    return S.procs.paywall_results[`${q.game_key}|${q.placement_id}|${q.test_id || ""}`] ?? S.procs.paywall_results[`${q.game_key}|${q.placement_id}|`] ?? { error: "No results in the demo data." }
  },
  admin_preview_paywall: (app: string, placement: string) => S.procs.previews[`${app}|${placement}`] ?? { error: "No preview for this build in the demo data." },
}

/** No identities are made in the demo. */
export function createIdentity(): never {
  throw new Error(READ_ONLY)
}

/** `/call/<name>`: the raw response text, like the server's. */
export function call(name: string, args: unknown[]): string {
  const f = PROCS[name]
  if (!f) throw new Error(READ_ONLY)
  return JSON.stringify(f(...(args as never[])))
}

// --- /route, /logs -------------------------------------------------------------------------------

export function route(path: string): { status: number; json: unknown } {
  if (path === "/healthz") return { status: 200, json: { status: "healthy", version: S.version } }
  // The demo signs in without a provider.
  if (path === "/admin/sign-in") return { status: 200, json: {} }
  return { status: 403, json: { error: READ_ONLY } }
}

export function logs(): string {
  const t = S.generated_at_ms * 1000
  const line = (ago: number, level: string, message: string) => JSON.stringify({ ts: t - ago * 1_000_000, level, target: "purser", message })
  return [
    line(3600, "Info", "worker: FX rates refreshed (ECB, 31 currencies)"),
    line(1820, "Info", "verify google com.example.galaxyminer gems_medium: verified, granted"),
    line(960, "Info", "notify apple com.example.galaxyminer.ios: DID_RENEW vip_monthly"),
    line(610, "Warn", "delivery com.example.wordgarden: webhook answered HTTP 503 (api.example.com), retry in 4 min"),
    line(300, "Info", "integration ga4 com.example.galaxyminer: purchase sent"),
    line(120, "Info", "verify stripe com.example.wordgarden.web plus_monthly: verified, granted"),
  ].join("\n")
}
