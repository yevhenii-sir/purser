import { useMemo, useState } from "react"
import { RefreshCwIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ErrorBox, Loading, PageHeader, Section } from "@/components/common"
import { DB_BASE, getToken } from "@/lib/api"
import { fmtTime } from "@/lib/format"
import { useAsync } from "@/lib/hooks"
import { cn } from "@/lib/utils"

/** A line of `/logs`: SpacetimeDB's JSON record, `ts` in microseconds since the epoch. */
interface LogLine {
  level?: string
  ts?: number
  message?: string
  [k: string]: unknown
}

/**
 * SpacetimeDB's `/call` route first looks for a reducer, logs this error and then runs the procedure of
 * that name: every procedure call made by this page (whoami, admin_revenue_stats, ...) leaves one.
 */
const CALL_NOISE = "External attempt to call nonexistent reducer"

export function LogsPage() {
  const [filter, setFilter] = useState("")
  const [showNoise, setShowNoise] = useState(false)
  const { data, error, loading, reload } = useAsync(async () => {
    let text: string
    if (__DEMO__) text = (await import("@/lib/demo")).logs()
    else {
      const res = await fetch(`${DB_BASE}/logs?num_lines=500`, { headers: { Authorization: "Bearer " + getToken() } })
      text = await res.text()
      if (!res.ok) throw new Error(text || "HTTP " + res.status)
    }
    return text
      .split("\n")
      .filter(Boolean)
      .map((l): LogLine => { try { return JSON.parse(l) } catch { return { message: l } } })
      .reverse()
  })
  const noise = useMemo(() => (data || []).filter((l) => String(l.message ?? "").includes(CALL_NOISE)).length, [data])
  const lines = useMemo(() => {
    const f = filter.toLowerCase()
    return (data || []).filter((l) =>
      (showNoise || !String(l.message ?? "").includes(CALL_NOISE)) && (!f || JSON.stringify(l).toLowerCase().includes(f)))
  }, [data, filter, showNoise])
  return (
    <>
      <PageHeader title="Logs" description="The last 500 lines of the module log, newest first."
        actions={<Button variant="outline" onClick={reload}><RefreshCwIcon /> Refresh</Button>} />
      <div className="flex flex-wrap items-center gap-x-6 gap-y-3">
        <Input placeholder="Filter" value={filter} onChange={(e) => setFilter(e.target.value)} className="max-w-sm" />
        <div className="flex items-center gap-2" title="SpacetimeDB logs these for every procedure called through /call; they are harmless">
          <Checkbox id="noise" checked={showNoise} onCheckedChange={(v) => setShowNoise(v === true)} />
          <Label htmlFor="noise" className="font-normal">Show "nonexistent reducer" notices of procedure calls ({noise})</Label>
        </div>
      </div>
      <Section>
        {error ? <ErrorBox error={error} /> : loading ? <Loading rows={8} /> : !lines.length ? (
          <div className="py-8 text-center text-sm text-muted-foreground">Nothing here</div>
        ) : (
          <div className="divide-y font-mono text-xs">
            {lines.map((l, i) => {
              const level = String(l.level || "").toUpperCase()
              const ts = l.ts ? fmtTime(l.ts) : ""
              return (
                <div key={i} className="flex gap-3 py-1.5">
                  <span className="shrink-0 text-muted-foreground">{ts}</span>
                  <span className={cn("w-12 shrink-0", level === "ERROR" && "text-red-600 dark:text-red-400", level === "WARN" && "text-amber-600 dark:text-amber-400")}>{level}</span>
                  <span className="min-w-0 break-words whitespace-pre-wrap">{l.message ?? JSON.stringify(l)}</span>
                </div>
              )
            })}
          </div>
        )}
      </Section>
    </>
  )
}
