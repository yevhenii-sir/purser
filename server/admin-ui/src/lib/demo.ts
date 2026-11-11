// Demo build of the admin page (`npm run build:demo`): no server, no sign-in. Reads answer from a
// snapshot of a database filled with made-up apps and purchases (`demo/snapshot.json`, made by
// tools/demo_snapshot.py); every change is refused. The clock is set back to the moment the
// snapshot was taken, so "last 30 days" and "2 minutes ago" mean what they meant then.

import snapshot from "../../demo/snapshot.json"
import { storeOf } from "./stores"
import type { GameApp } from "./types"

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
export const DEMO_DB = "billing-demo"
export const DEMO_TAKEN_AT = S.generated_at_ms

/** Sets the page clock back to when the snapshot was taken (call before anything reads the time). */
export function installClock() {
  const RealDate = Date
  const offset = RealDate.now() - S.generated_at_ms
  class DemoDate extends RealDate {
    constructor(...args: any[]) {
      if (args.length === 0) super(RealDate.now() - offset)
      else super(...(args as [any]))
    }
    static now() {
      return RealDate.now() - offset
    }
  }
  ;(globalThis as any).Date = DemoDate
}

// --- /sql ----------------------------------------------------------------------------------------

/** The few query shapes the page uses: `SELECT * | cols | COUNT(*) AS n FROM t [WHERE c = v]`. */
export function sql(query: string): Row[] {
  const m = /^\s*SELECT\s+(.+?)\s+FROM\s+(\w+)(?:\s+WHERE\s+(\w+)\s*=\s*('(?:[^']|'')*'|[\w.x-]+))?\s*$/is.exec(query)
  if (!m) throw new Error(READ_ONLY)
  const [, cols, table, col, raw] = m
  let rows = S.tables[table] || []
  if (col) {
    const v = raw.startsWith("'") ? raw.slice(1, -1).replace(/''/g, "'") : raw
    rows = rows.filter((r) => String(r[col]) === v)
  }
  if (/^COUNT\(\*\)\s+AS\s+(\w+)$/i.test(cols.trim())) return [{ [cols.trim().split(/\s+/).pop()!]: rows.length }]
  if (cols.trim() === "*") return rows.map((r) => ({ ...r }))
  const names = cols.split(",").map((c) => c.trim())
  return rows.map((r) => Object.fromEntries(names.map((n) => [n, r[n]])))
}

// --- /call ---------------------------------------------------------------------------------------

const isoDay = (ms: number) => new Date(ms).toISOString().slice(0, 10)

function revenue(json: string) {
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
    .sort((a, b) => Number(b.created_at_micros) - Number(a.created_at_micros))
  const limit = Number(q.limit) || 200
  return { rows: rows.slice(0, limit), more: rows.length > limit }
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
  return { rows, apple_groups: Object.fromEntries(apple.map((a: Row) => [a.app_id, ["Premium"]])), warnings: {} }
}

const PROCS: Record<string, (...args: any[]) => unknown> = {
  admin_store_create_preview: storeCreatePreview,
  whoami: () => "0x" + S.identity,
  admin_me: () => ({ identity: S.identity, name: S.tables.admin?.find((a: Row) => a.protected)?.name || "", role: "owner", owner: true }),
  admin_overview: () => S.procs.admin_overview,
  admin_revenue_stats: revenue,
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
  return JSON.stringify(f(...args))
}

// --- /route, /logs -------------------------------------------------------------------------------

export function route(path: string): { status: number; json: any } {
  if (path === "/healthz") return { status: 200, json: { status: "healthy", version: S.version } }
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
