/** Subscription billing periods (ISO 8601) every store that creates subscriptions supports
 * (`purser_core::store_create::PERIODS`), with their labels. */
export const PERIODS: [string, string][] = [["P1W", "Weekly"], ["P1M", "Monthly"], ["P3M", "Every 3 months"], ["P6M", "Every 6 months"], ["P1Y", "Yearly"]]

const PER: Record<string, string> = { P1W: "week", P1M: "month", P3M: "3 months", P6M: "6 months", P1Y: "year" }

/** `P1M` → `month` (for "$4.99 / month"); "" for an unknown period. */
export const perPeriod = (iso: string | undefined) => PER[iso || ""] || ""

/** The period of a store catalog row (`billing_period_days`, as `purser_core::catalog::period_days` counts
 * them) when it is one of [`PERIODS`]; "" otherwise. */
export function periodOfDays(days: number): string {
  return ({ 7: "P1W", 30: "P1M", 90: "P3M", 180: "P6M", 365: "P1Y" } as Record<number, string>)[Number(days)] || ""
}
