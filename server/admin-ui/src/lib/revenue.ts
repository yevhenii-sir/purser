import { proc } from "./api"
import { cached, forget } from "./hooks"
import type { RevenueOverview, Stats } from "./types"

/** Revenue reports are kept this long in the session, so switching pages does not recompute them. */
const TTL_MS = 30_000
const PREFIX = "revenue:"

/** The revenue report for `query` (see `admin_revenue_stats`). `fresh`: a manual refresh, which
 * also drops the other cached reports (a rebuild or a rate change touches all of them) and asks the
 * server to count a kept report again (it does so at most once a minute). */
export function revenueStats(query: Record<string, unknown>, fresh = false): Promise<Stats> {
  if (fresh) forget(PREFIX)
  const json = JSON.stringify(query)
  return cached(PREFIX + json, TTL_MS, () => proc<Stats>("admin_revenue_stats", fresh ? JSON.stringify({ ...query, refresh: true }) : json))
}

/** Totals of the last 30 days for the Overview and the Apps list. The server keeps the report for
 * an hour and counts it again only when a page asks after that; `refresh` counts it now (the
 * server allows that once a minute and answers the kept report otherwise). */
export function revenueOverview(refresh = false): Promise<RevenueOverview> {
  return cached("revenue-overview", TTL_MS, () => proc<RevenueOverview>("admin_revenue_overview", JSON.stringify({ refresh })), refresh)
}
