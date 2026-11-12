export const fmtTime = (micros: number | string | undefined) =>
  micros && Number(micros) ? new Date(Number(micros) / 1000).toLocaleString() : "—"

export const fmtMs = (ms: number | string | undefined) => (ms && Number(ms) ? new Date(Number(ms)).toLocaleString() : "—")

export const usd = (micros: number) =>
  (Number(micros) < 0 ? "−$" : "$") +
  (Math.abs(Number(micros)) / 1e6).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })

export const money = (micros: number, currency: string) =>
  micros ? `${(Number(micros) / 1e6).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ${currency}` : "—"

/** "just now", "5 min ago", "3 h ago", "2 days ago" for a time in microseconds. */
export function ago(micros: number | string): string {
  const s = Math.max(0, (Date.now() - Number(micros) / 1000) / 1000)
  if (s < 60) return "just now"
  if (s < 3600) return `${Math.floor(s / 60)} min ago`
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`
  const d = Math.floor(s / 86400)
  return d === 1 ? "yesterday" : `${d} days ago`
}

export const shortDay = (day: string) =>
  new Date(day + "T00:00:00Z").toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" })

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/** A fraction as a percentage ("12.5%"); "—" when unknown. */
export const pct = (v: number | null | undefined, digits = 1) => (v === null || v === undefined ? "—" : `${(v * 100).toFixed(digits)}%`)
