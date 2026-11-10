import { useState } from "react"
import { DataTable, ErrorBox, Loading, Mono, PageHeader, Section, Stat, StatusBadge, Tone } from "@/components/common"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { proc, route } from "@/lib/api"
import { fmtTime, usd } from "@/lib/format"
import { useAsync } from "@/lib/hooks"
import { revenueSummary } from "@/lib/revenue"
import { STATUSES, type Overview } from "@/lib/types"
import type { PageProps } from "@/routes"

export function OverviewPage({ go }: PageProps) {
  const [open, setOpen] = useState<string | null>(null)
  const { data, error, loading, reload } = useAsync(async ({ fresh }) => {
    const [health, overview, stats] = await Promise.all([
      route("/healthz"),
      proc<Overview>("admin_overview"),
      revenueSummary(fresh).catch(() => null),
    ])
    const byStatus = STATUSES.map((s) => ({ status: s, n: Number(overview.by_status[s] || 0) }))
    // Statuses the page does not know yet still show up.
    for (const [s, n] of Object.entries(overview.by_status)) if (!STATUSES.includes(s)) byStatus.push({ status: s, n: Number(n) })
    return { health, overview, byStatus, stats }
  })

  if (error) return <ErrorBox error={error} hint="Make sure the token belongs to the database owner or an admin." />
  if (loading && !data) return <Loading rows={6} />
  if (!data) return null
  const { health, stats, overview: ov } = data
  return (
    <>
      <PageHeader title="Overview" description="Server status, the ledger and what the worker still has to finish." />
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <Stat
          label="Server"
          value={health.status === 200 ? <Tone tone="ok">healthy</Tone> : <Tone tone="bad">{health.status}</Tone>}
          hint={health.json?.version ? `module ${health.json.version}` : undefined}
        />
        <Stat label="Net revenue, 30 days" value={stats ? usd(stats.totals.net_usd_micros) : "—"} hint={stats?.rebuilding ? "USD, totals are being rebuilt" : "USD, without sandbox"} onClick={() => go("revenue")} />
        <Stat label="Apps / products" value={`${ov.apps} / ${ov.products}`} onClick={() => go("apps")} />
        <Stat label="Purchases" value={Number(ov.total).toLocaleString()} hint={`${ov.pending_total} waiting · ${ov.outbox} events queued`} onClick={() => go("purchases")} />
      </div>
      <div className="grid gap-6 lg:grid-cols-[1fr_2fr]">
        <Section title="Purchases by status">
          <DataTable
            rows={data.byStatus.filter((s) => s.n > 0 || ["completed", "verified"].includes(s.status))}
            columns={[
              { title: "Status", cell: (r) => <StatusBadge status={r.status} /> },
              { title: "Count", cell: (r) => r.n.toLocaleString(), num: true },
            ]}
          />
        </Section>
        <Section
          title="Waiting for processing or retry"
          description="Every minute the worker finishes reward delivery, consume / acknowledge and revocations. This is what is stuck and why."
        >
          <DataTable
            rows={ov.pending}
            onRowClick={(r) => setOpen(r.id)}
            empty="All processed"
            columns={[
              { title: "Purchase", cell: (r) => <Mono>{r.id}</Mono> },
              { title: "Status", cell: (r) => <StatusBadge status={r.status} /> },
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
