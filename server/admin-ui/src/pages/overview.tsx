import { useState } from "react"
import { TriangleAlertIcon } from "lucide-react"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { DataTable, ErrorBox, Loading, Mono, PageHeader, Section, Stat, StatusBadge, Tone } from "@/components/common"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc, route } from "@/lib/api"
import { fmtTime, usd } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { useCan } from "@/lib/roles"
import { revenueOverview, revenueOverviewIfOn } from "@/lib/revenue"
import { RevenueUpdated } from "@/components/revenue-updated"
import { STATUSES, type Overview, type PurchaseStatus, type RevenueOverview, type Waiting } from "@/lib/types"
import type { PageProps } from "@/routes"

/** Listed even when no purchase is in them. */
const ALWAYS_LISTED: readonly PurchaseStatus[] = ["completed", "verified"]

export function OverviewPage({ go }: PageProps) {
  const [open, setOpen] = useState<string | null>(null)
  /** A report counted again with the refresh button (newer than the one loaded with the page). */
  const [counted, setCounted] = useState<RevenueOverview | null>(null)
  const { data, error, loading, reload } = useAsync(async () => {
    const [health, overview, stats] = await Promise.all([
      route<{ version?: string } | null>("/healthz"),
      proc<Overview>("admin_overview"),
      revenueOverviewIfOn(),
    ])
    const byStatus = STATUSES.map((s) => ({ status: s, n: Number(overview.by_status[s] || 0) }))
    return { health, overview, byStatus, stats }
  })

  if (error) return <ErrorBox error={error} hint="Make sure the token belongs to the database owner or an admin." />
  if (loading && !data) return <Loading rows={6} />
  if (!data) return null
  const { health, overview: ov } = data
  const analyticsOff = data.stats === "off"
  const loadedStats = data.stats === "off" ? null : data.stats
  const stats = counted && counted.computed_at_micros > (loadedStats?.computed_at_micros ?? 0) ? counted : loadedStats
  const stopped = ov.worker_age_secs !== null && ov.worker_age_secs > ov.worker_stale_after_secs
  const waitedSecs = ov.due_total > 0 ? Math.max(0, Math.round((Date.now() * 1000 - Number(ov.oldest_due_micros)) / 1e6)) : 0
  const behind = !stopped && waitedSecs >= ov.backlog_after_secs
  return (
    <>
      <PageHeader title="Overview" description="Server status, the ledger and what the worker still has to finish." />
      {stopped && <WorkerStopped minutes={Math.round(Number(ov.worker_age_secs) / 60)} onRestarted={reload} />}
      {behind && <QueueBehind due={ov.due_total} minutes={Math.round(waitedSecs / 60)} drainers={ov.drainers} lanes={ov.lanes} />}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <Stat
          label="Server"
          value={health.status === 200 ? <Tone tone="ok">healthy</Tone> : <Tone tone="bad">{health.status}</Tone>}
          hint={health.json?.version ? `module ${health.json.version}` : undefined}
        />
        <Stat label="Net revenue, 30 days" value={stats ? usd(stats.totals.net_usd_micros) : "—"} hint={analyticsOff ? "revenue analytics are off (Server → Analytics)" : <>{stats?.rebuilding ? "USD, totals are being rebuilt" : "USD, without sandbox"}{stats && " · "}<RevenueUpdated stats={stats} onRefresh={async () => setCounted(await revenueOverview(true))} /></>} onClick={() => go(analyticsOff ? "server" : "revenue")} />
        <Stat label="Apps / builds" value={`${ov.apps} / ${ov.builds}`} onClick={() => go("apps")} />
        <Stat label="Purchases" value={Number(ov.total).toLocaleString()} hint={`${ov.pending_total} waiting (${ov.due_total} due) · ${ov.outbox} events queued`} onClick={() => go("purchases")} />
      </div>
      <div className="grid gap-6 lg:grid-cols-[1fr_2fr]">
        <Section title="Purchases by status">
          <DataTable
            rows={data.byStatus.filter((s) => s.n > 0 || ALWAYS_LISTED.includes(s.status))}
            columns={[
              { title: "Status", cell: (r) => <StatusBadge status={r.status} /> },
              { title: "Count", cell: (r) => r.n.toLocaleString(), num: true },
            ]}
          />
        </Section>
        <Section
          title="Waiting for processing or retry"
          description={`Queue drainers (${ov.drainers} of ${ov.lanes} running) finish reward delivery, consume / acknowledge and revocations a request left, and retry failed steps. This is what waits and why.`}
        >
          <DataTable
            rows={ov.pending}
            onRowClick={(r) => setOpen(r.id)}
            empty="All processed"
            columns={[
              { title: "Purchase", cell: (r) => <Mono>{r.id}</Mono> },
              { title: "Status", cell: (r) => <StatusBadge status={r.status} /> },
              { title: "Waiting", cell: (r) => <WaitingBadge waiting={r.waiting} /> },
              { title: "Attempts", cell: (r) => r.attempts, num: true },
              { title: "Next try", cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.next_attempt_at_micros)}</span> },
              { title: "Error", className: "whitespace-normal", cell: (r) => <Mono className="line-clamp-2 break-all">{r.last_error}</Mono> },
            ]}
          />
          {ov.pending_total > ov.pending.length && (
            <p className="mt-3 text-xs text-muted-foreground">The {ov.pending.length} soonest of {ov.pending_total}.</p>
          )}
        </Section>
      </div>
      <PurchaseSheet id={open} onClose={(changed) => { setOpen(null); if (changed) reload() }} />
    </>
  )
}

const WAITING: Record<Waiting, { label: string; tone: "ok" | "warn" | "bad" | "muted"; hint: string }> = {
  running: { label: "running", tone: "ok", hint: "A request or a queue drainer is running its step now." },
  blocked: {
    label: "blocked",
    tone: "bad",
    hint: "Cannot work until the setup is fixed (see the error). It is tried again as soon as the build, its keys or the delivery hosts change.",
  },
  game_answer: { label: "game's answer", tone: "warn", hint: "Sent to the game database; it has not answered yet." },
  due: { label: "due", tone: "muted", hint: "Due now: the next free queue drainer takes it." },
  retry: { label: "retry", tone: "warn", hint: "The last attempt failed; it is tried again at the next try." },
  later: { label: "later", tone: "muted", hint: "Left for later: its store or game did not answer in time." },
}

function WaitingBadge({ waiting }: { waiting: Waiting | undefined }) {
  const w = waiting && WAITING[waiting]
  if (!w) return null
  return (
    <span title={w.hint}>
      <Tone tone={w.tone}>{w.label}</Tone>
    </span>
  )
}

/** Purchases have waited in the queue longer than `backlog_after_secs` after they became due. */
function QueueBehind({ due, minutes, drainers, lanes }: { due: number; minutes: number; drainers: number; lanes: number }) {
  return (
    <Alert>
      <TriangleAlertIcon />
      <AlertTitle>The purchase queue is behind: {due.toLocaleString()} due, the oldest for {minutes} minutes</AlertTitle>
      <AlertDescription>
        Players wait for their rewards. The errors below say why: usually a slow store or game server. {drainers} of {lanes} queue
        drainers run; the worker_lanes setting allows more at once.
      </AlertDescription>
    </Alert>
  )
}

/** The ledger worker has not run for a while: purchases, renewals and alerts wait. */
function WorkerStopped({ minutes, onRestarted }: { minutes: number; onRestarted: () => void }) {
  const canRestart = useCan()("configure")
  const restart = async () => {
    const ok = await tryAct(() => call("admin_ensure_worker"), "Worker scheduled")
    if (ok !== undefined) onRestarted()
  }
  return (
    <Alert variant="destructive">
      <TriangleAlertIcon />
      <AlertTitle>The worker stopped {minutes} minutes ago</AlertTitle>
      <AlertDescription>
        Purchases are not retried, subscriptions are not re-checked and owner alerts are not sent until it runs again. /healthz answers 503.
        {canRestart && <Button size="sm" variant="outline" className="mt-2" onClick={restart}>Restart the worker</Button>}
      </AlertDescription>
    </Alert>
  )
}
