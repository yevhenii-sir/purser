import { useState } from "react"
import { TriangleAlertIcon } from "lucide-react"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { RichTable } from "@/components/rich-table"
import { DataTable, ErrorBox, Loading, Mono, PageHeader, Section, Stat, StatusBadge, Tone } from "@/components/common"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc, route, select } from "@/lib/api"
import { useAppIndex } from "@/lib/apps"
import { ruleTitle } from "@/pages/alerts"
import { fmtTime, usd } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { useCan } from "@/lib/roles"
import { revenueOverview, revenueOverviewIfOn } from "@/lib/revenue"
import { RevenueUpdated } from "@/components/revenue-updated"
import { STATUSES, type Overview, type PurchaseStatus, type RevenueOverview, type Waiting } from "@/lib/types"
import { appRoute, type PageProps } from "@/routes"

/** Listed even when no purchase is in them. */
const ALWAYS_LISTED: readonly PurchaseStatus[] = ["completed", "verified"]

export function OverviewPage({ go }: PageProps) {
  const [open, setOpen] = useState<string | null>(null)
  /** A report counted again with the refresh button (newer than the one loaded with the page). */
  const [counted, setCounted] = useState<RevenueOverview | null>(null)
  const { data, error, loading, reload } = useAsync(async () => {
    const [health, overview, stats, alerts, checks] = await Promise.all([
      route<{ version?: string } | null>("/healthz"),
      proc<Overview>("admin_overview"),
      revenueOverviewIfOn(),
      // An admin without access to these sees the rest of the page.
      select<OpenAlert>({ table: "alert_state" }).catch(() => [] as OpenAlert[]),
      select<SetupCheck>({ table: "app_check", columns: ["app_id", "summary", "checked_at_micros"] }).catch(() => [] as SetupCheck[]),
    ])
    const byStatus = STATUSES.map((s) => ({ status: s, n: Number(overview.by_status[s] || 0) }))
    return { health, overview, byStatus, stats, alerts, checks }
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
      <div className="grid gap-6 lg:grid-cols-[minmax(14rem,1fr)_3fr]">
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
              {
                title: "Purchase",
                className: "max-w-80",
                cell: (r) => (
                  <div className="min-w-0">
                    <div className="truncate font-mono text-xs" title={r.id}>{r.id}</div>
                    {r.last_error && <div className="truncate text-xs text-destructive" title={r.last_error}>{r.last_error}</div>}
                  </div>
                ),
              },
              { title: "Status", cell: (r) => <StatusBadge status={r.status} /> },
              { title: "Waiting", cell: (r) => <WaitingBadge waiting={r.waiting} /> },
              { title: "Attempts", cell: (r) => r.attempts, num: true },
              { title: "Next try", cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.next_attempt_at_micros)}</span> },
            ]}
          />
          {ov.pending_total > ov.pending.length && (
            <p className="mt-3 text-xs text-muted-foreground">The {ov.pending.length} soonest of {ov.pending_total}.</p>
          )}
        </Section>
      </div>
      <Attention alerts={data.alerts} checks={data.checks} go={go} />
      <PurchaseSheet id={open} onClose={(changed) => { setOpen(null); if (changed) reload() }} />
    </>
  )
}

interface OpenAlert { key: string; rule: string; app_id: string; active: boolean; first_at_micros: number; detail: string }
interface SetupCheck { app_id: string; summary: string; checked_at_micros: number }

/** Open owner alerts and builds whose last setup check found problems: what to look at first. */
function Attention({ alerts, checks, go }: { alerts: OpenAlert[]; checks: SetupCheck[]; go: PageProps["go"] }) {
  const { groupOf } = useAppIndex()
  const [query, setQuery] = useState("")
  const rows = [
    ...alerts
      .filter((a) => a.active)
      .sort((a, b) => Number(b.first_at_micros) - Number(a.first_at_micros))
      .map((a) => ({ key: `a/${a.key}`, kind: "alert" as const, app: a.app_id, title: ruleTitle(a.rule), detail: a.detail, at: Number(a.first_at_micros) })),
    ...checks
      .filter((c) => c.summary === "fail" || c.summary === "warn")
      .sort((a, b) => (a.summary === b.summary ? 0 : a.summary === "fail" ? -1 : 1))
      .map((c) => ({
        key: `c/${c.app_id}`,
        kind: c.summary as "fail" | "warn",
        app: c.app_id,
        title: c.summary === "fail" ? "Setup check fails" : "Setup check warns",
        detail: "open the build's Setup tab for the checks",
        at: Number(c.checked_at_micros),
      })),
  ]
  // With hundreds of builds the list is long: a filter by build, app, problem or detail.
  const q = query.trim().toLowerCase()
  const shown = q ? rows.filter((r) => `${r.title} ${r.app} ${groupOf.get(r.app)?.name || ""} ${r.detail}`.toLowerCase().includes(q)) : rows
  const open = (r: (typeof rows)[number]) => {
    if (r.kind === "alert") return go("alerts")
    const g = groupOf.get(r.app)
    if (g) location.hash = appRoute(g.key, "setup")
  }
  return (
    <Section title="Needs attention" description="Open owner alerts and builds whose last setup check found problems. Click a row to open it.">
      <RichTable
        rows={shown}
        rowKey={(r) => r.key}
        onRowClick={open}
        noun="problems"
        pageSize={10}
        empty={rows.length ? "No problem matches." : "Nothing: no open alerts, every setup check passes."}
        toolbar={rows.length > 10 && <Input className="h-8 w-64" placeholder="Filter by build, app or problem" value={query} onChange={(e) => setQuery(e.target.value)} />}
        columns={[
          { title: "", id: "kind", size: 70, sort: (r) => ["alert", "fail", "warn"].indexOf(r.kind), cell: (r) => <Tone tone={r.kind === "warn" ? "warn" : "bad"}>{r.kind === "alert" ? "alert" : r.kind}</Tone> },
          { title: "What", sort: (r) => r.title, cell: (r) => <span className="font-medium">{r.title}</span> },
          { title: "Build", sort: (r) => r.app, cell: (r) => <Mono>{r.app || "server"}</Mono> },
          { title: "Details", cell: (r) => <span className="block max-w-96 truncate text-xs text-muted-foreground" title={r.detail}>{r.detail}</span> },
          { title: "Since", sort: (r) => r.at, cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.at)}</span> },
        ]}
      />
    </Section>
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
