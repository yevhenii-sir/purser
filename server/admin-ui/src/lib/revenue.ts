import { proc } from "./api"
import { cached, forget } from "./hooks"
import type { Stats, StatsSummary } from "./types"

/** Revenue reports are kept this long in the session, so switching pages does not recompute them. */
const TTL_MS = 30_000
const PREFIX = "revenue:"

/** The revenue report for `query` (see `admin_revenue_stats`). `fresh`: a manual refresh, which
 * also drops the other cached reports (a rebuild or a rate change touches all of them). */
export function revenueStats(query: Record<string, unknown>, fresh = false): Promise<Stats> {
  if (fresh) forget(PREFIX)
  const json = JSON.stringify(query)
  return cached(PREFIX + json, TTL_MS, () => proc<Stats>("admin_revenue_stats", json))
}

/** Totals of the last 30 days for the Overview and the Apps list (no ledger scans on the server). */
export function revenueSummary(fresh = false): Promise<StatsSummary> {
  if (fresh) forget(PREFIX)
  const json = JSON.stringify({ summary: true })
  return cached(PREFIX + json, TTL_MS, () => proc<StatsSummary>("admin_revenue_stats", json))
}
