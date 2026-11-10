import { useMemo, useState } from "react"
import { Bar, BarChart, CartesianGrid, Line, LineChart, ReferenceLine, XAxis, YAxis } from "recharts"
import { DownloadIcon, PercentIcon, PlusIcon, RefreshCwIcon, RotateCcwIcon, TriangleAlertIcon, XIcon } from "lucide-react"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { ChartContainer, ChartTooltip, ChartTooltipContent, type ChartConfig } from "@/components/ui/chart"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, PageHeader, Section, Stat } from "@/components/common"
import { RevenueTree } from "@/components/revenue-tree"
import { RichTable, type RichColumn } from "@/components/rich-table"
import { call, proc } from "@/lib/api"
import { useApps } from "@/lib/apps"
import { money, shortDay, usd } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { revenueStats } from "@/lib/revenue"
import type { Cohort, Money, Stats } from "@/lib/types"
import type { PageProps } from "@/routes"

const RANGES = [["7", "Last 7 days"], ["30", "Last 30 days"], ["90", "Last 90 days"], ["365", "Last 365 days"]]
const ALL = "__all__"

const netConfig = { net: { label: "Net revenue", color: "var(--chart-1)" } } satisfies ChartConfig
const subsConfig = { active: { label: "Active subscriptions", color: "var(--chart-2)" } } satisfies ChartConfig

const isoDay = (d: Date) => d.toISOString().slice(0, 10)
const compactUsd = (micros: number) =>
  "$" + (micros / 1e6).toLocaleString(undefined, { notation: "compact", maximumFractionDigits: 1 })
const pct = (v: number | null | undefined) => (v === null || v === undefined ? "—" : `${(v * 100).toFixed(1)}%`)

const regionNames = (() => {
  try {
    return new Intl.DisplayNames(undefined, { type: "region" })
  } catch {
    return null
  }
})()
const countryName = (code: string) => {
  if (!code) return "Unknown"
  try {
    return regionNames?.of(code) || code
  } catch {
    return code
  }
}

/** Saves rows as a CSV file (values with commas or quotes are quoted). */
function downloadCsv(name: string, rows: (string | number | null)[][]) {
  const cell = (v: string | number | null) => {
    const s = v === null ? "" : String(v)
    return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s
  }
  const a = document.createElement("a")
  a.href = URL.createObjectURL(new Blob([rows.map((r) => r.map(cell).join(",")).join("\n")], { type: "text/csv" }))
  a.download = name
  a.click()
  setTimeout(() => URL.revokeObjectURL(a.href), 1000)
}

function CsvButton({ onClick }: { onClick: () => void }) {
  return <Button variant="outline" size="sm" onClick={onClick}><DownloadIcon /> CSV</Button>
}

function moneyColumns<T extends Money>(): RichColumn<T>[] {
  return [
    { title: "Gross", sort: (x) => x.gross_usd_micros, cell: (x) => usd(x.gross_usd_micros), num: true },
    { title: "Refunds", sort: (x) => x.refunded_usd_micros, cell: (x) => usd(x.refunded_usd_micros), num: true },
    { title: "Net", sort: (x) => x.net_usd_micros, cell: (x) => <span className="font-medium">{usd(x.net_usd_micros)}</span>, num: true },
    { title: "Proceeds*", sort: (x) => x.proceeds_usd_micros, cell: (x) => usd(x.proceeds_usd_micros), num: true },
    { title: "Purchases", sort: (x) => x.purchases, cell: (x) => x.purchases, num: true },
    { title: "New subs", sort: (x) => x.new_subscriptions, cell: (x) => x.new_subscriptions, num: true },
    { title: "Renewals", sort: (x) => x.renewals, cell: (x) => x.renewals, num: true },
    { title: "Refunds #", sort: (x) => x.refunds, cell: (x) => x.refunds, num: true },
  ]
}

const payments = (m: Money) => m.purchases + m.new_subscriptions + m.renewals

/** `fixedGame`: only this app (the app page); `embedded`: no page header. */
export function RevenuePage({ fixedGame, embedded }: PageProps & { fixedGame?: string; embedded?: boolean }) {
  const [days, setDays] = useState("30")
  const [game, setGame] = useState(fixedGame || ALL)
  const [sandbox, setSandbox] = useState(false)
  const [fxDialog, setFxDialog] = useState<string | null>(null)
  const [feeDialog, setFeeDialog] = useState(false)

  // The game filter of the Revenue page; the app page passes its app instead.
  const apps = useApps().apps
  const games = useMemo(() => (fixedGame ? [] : [...new Set((apps || []).map((a) => a.game_key))].filter(Boolean).sort()), [fixedGame, apps])
  const { data: r, error, loading, reload } = useAsync(async ({ fresh }) => {
    const to = new Date()
    const from = new Date(to.getTime() - (Number(days) - 1) * 86_400_000)
    const q = { from: isoDay(from), to: isoDay(to), include_sandbox: sandbox, game_key: game === ALL ? "" : game }
    return revenueStats(q, fresh)
  }, [days, game, sandbox])

  const exportCsv = () => {
    if (!r) return
    const cols = ["day", "gross_usd_micros", "refunded_usd_micros", "net_usd_micros", "store_fee_usd_micros", "proceeds_usd_micros", "purchases", "new_subscriptions", "renewals", "refunds", "transfers", "expirations", "trials", "trial_conversions"] as const
    downloadCsv(`revenue_${r.from}_${r.to}.csv`, [[...cols], ...r.series.map((s) => cols.map((c) => s[c]))])
  }

  const subs = r?.subscriptions
  const hasSubs = !!subs && (subs.active_now > 0 || subs.new_subscriptions + subs.trials_started + subs.renewals + subs.expirations > 0)

  return (
    <>
      {!embedded && (
        <PageHeader
          title="Revenue"
          description="Store prices before the store's commission, converted to USD at the ECB rate of each day. Proceeds are an estimate."
        />
      )}
      <div className="flex flex-wrap items-center gap-3">
        <Select value={days} onValueChange={setDays}>
          <SelectTrigger className="w-40"><SelectValue /></SelectTrigger>
          <SelectContent>{RANGES.map(([v, l]) => <SelectItem key={v} value={v}>{l}</SelectItem>)}</SelectContent>
        </Select>
        {!fixedGame && (
          <Select value={game} onValueChange={setGame}>
            <SelectTrigger className="w-44"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>All apps</SelectItem>
              {games.map((g) => <SelectItem key={g} value={g}>{g}</SelectItem>)}
            </SelectContent>
          </Select>
        )}
        <div className="flex items-center gap-2">
          <Switch id="sandbox" checked={sandbox} onCheckedChange={setSandbox} />
          <Label htmlFor="sandbox" className="font-normal">Include sandbox</Label>
        </div>
        <div className="ms-auto flex flex-wrap items-center gap-2">
          <Button variant="outline" size="sm" onClick={exportCsv} disabled={!r}><DownloadIcon /> CSV</Button>
          <Button variant="outline" size="sm" onClick={() => setFeeDialog(true)} disabled={!r}><PercentIcon /> Store fee</Button>
          <Button variant="outline" size="sm" onClick={() => tryAct(() => proc("admin_refresh_fx"), "Rates updated").then(reload)}><RefreshCwIcon /> Rates</Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => confirm("Recompute all revenue totals from the purchase ledger?") && tryAct(() => call("admin_rebuild_revenue"), "Rebuild started").then(reload)}
          >
            <RotateCcwIcon /> Rebuild
          </Button>
        </div>
      </div>

      {error ? (
        <ErrorBox error={error} />
      ) : loading || !r || !subs ? (
        <Loading rows={6} />
      ) : (
        <>
          {r.rebuilding && (
            <Alert>
              <RotateCcwIcon />
              <AlertTitle>Revenue totals are being rebuilt</AlertTitle>
              <AlertDescription>The worker recomputes them from the purchase ledger in the background; numbers may be incomplete until it finishes.</AlertDescription>
            </Alert>
          )}
          {r.fx.missing.length > 0 && (
            <Alert>
              <TriangleAlertIcon />
              <AlertTitle>No exchange rate for {r.fx.missing.join(", ")}</AlertTitle>
              <AlertDescription>
                These sales are left out of the USD totals until you set a rate.
                <Button size="sm" variant="outline" className="mt-2" onClick={() => setFxDialog(r.fx.missing[0])}>Set rate</Button>
              </AlertDescription>
            </Alert>
          )}
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
            <Stat label="Net revenue" value={usd(r.totals.net_usd_micros)} hint={`gross ${usd(r.totals.gross_usd_micros)}`} />
            <Stat label="Refunds" value={usd(r.totals.refunded_usd_micros)} hint={`${r.totals.refunds} · ${(r.totals.refund_rate * 100).toFixed(1)}% of payments`} />
            <Stat label="Active subscriptions" value={subs.active_now} hint={`MRR ${usd(subs.mrr_usd_micros)}`} />
            <Stat label="Payments" value={payments(r.totals)}
              hint={`${r.totals.purchases} purchases · ${r.totals.new_subscriptions} new subs · ${r.totals.renewals} renewals`} />
            <Stat
              label="Proceeds (estimate)"
              value={usd(r.totals.proceeds_usd_micros)}
              hint={`after ~${usd(r.totals.store_fee_usd_micros)} store fee (${r.proceeds.percent}% default)`}
              onClick={() => setFeeDialog(true)}
            />
            <Stat label="New subscriptions" value={subs.new_subscriptions + subs.trial_conversions}
              hint={`${subs.new_subscriptions} paid starts · ${subs.trial_conversions} from trials · ${subs.trials_started} trials started`} />
            <Stat label="Trial → paid" value={pct(r.trials.conversion_rate)}
              hint={`${r.trials.converted} of ${r.trials.started - r.trials.pending} ended trials · ${r.trials.pending} running`} />
            <Stat label="Renewal rate" value={pct(subs.renewal_rate)}
              hint={`churn ${pct(subs.churn_rate)} a month · ${subs.expirations} expired`} />
          </div>

          <RevenueTree days={Number(days)} gameKey={game === ALL ? "" : game} sandbox={sandbox} refreshKey={r} />

          <Section title="Net revenue per day" description="Hover a day for gross, refunds, estimated proceeds and payments.">
            <ChartContainer config={netConfig} className="aspect-auto h-64 w-full">
              <BarChart data={r.series.map((s) => ({ ...s, net: s.net_usd_micros }))} margin={{ left: 4, right: 4 }}>
                <CartesianGrid vertical={false} />
                <XAxis dataKey="day" tickLine={false} axisLine={false} tickMargin={8} minTickGap={24} tickFormatter={shortDay} />
                <YAxis tickLine={false} axisLine={false} width={56} tickFormatter={compactUsd} />
                <ReferenceLine y={0} stroke="var(--border)" />
                <ChartTooltip
                  cursor={{ fill: "var(--muted)", opacity: 0.5 }}
                  content={
                    <ChartTooltipContent
                      labelFormatter={(_, p) => {
                        const s = p?.[0]?.payload
                        return s ? `${shortDay(s.day)} · ${payments(s)} payments` : ""
                      }}
                      formatter={(_, __, item) => {
                        const s = item.payload
                        return (
                          <div className="grid gap-0.5">
                            <span className="font-medium">{usd(s.net_usd_micros)} net</span>
                            <span className="text-muted-foreground">gross {usd(s.gross_usd_micros)} · refunds {usd(s.refunded_usd_micros)}</span>
                            <span className="text-muted-foreground">proceeds ~{usd(s.proceeds_usd_micros)}</span>
                          </div>
                        )
                      }}
                    />
                  }
                />
                <Bar dataKey="net" fill="var(--color-net)" radius={[4, 4, 0, 0]} />
              </BarChart>
            </ChartContainer>
          </Section>

          {subs.series.length > 0 && (
            <Section title="Active subscriptions" description="A snapshot is taken once a day, so the history starts on the day this version was installed.">
              <ChartContainer config={subsConfig} className="aspect-auto h-48 w-full">
                <LineChart data={subs.series} margin={{ left: 4, right: 4 }}>
                  <CartesianGrid vertical={false} />
                  <XAxis dataKey="day" tickLine={false} axisLine={false} tickMargin={8} minTickGap={24} tickFormatter={shortDay} />
                  <YAxis tickLine={false} axisLine={false} width={40} allowDecimals={false} />
                  <ChartTooltip content={<ChartTooltipContent labelFormatter={(_, p) => (p?.[0] ? shortDay(p[0].payload.day) : "")} />} />
                  <Line dataKey="active" type="monotone" stroke="var(--color-active)" strokeWidth={2} dot={false} />
                </LineChart>
              </ChartContainer>
            </Section>
          )}

          {hasSubs && (
            <Section
              title="Subscriptions by month"
              description="Churn: expired ÷ active at the first snapshot of the month. Renewal rate: renewals ÷ (renewals + expirations); renewals include the first paid period after a trial."
              actions={
                <CsvButton
                  onClick={() =>
                    downloadCsv(`subscriptions_${r.from}_${r.to}.csv`, [
                      ["month", "active_start", "new_subscriptions", "trials", "trial_conversions", "renewals", "expirations", "churn_rate", "renewal_rate"],
                      ...subs.monthly.map((m) => [m.month, m.active_start, m.new_subscriptions, m.trials, m.trial_conversions, m.renewals, m.expirations, m.churn_rate, m.renewal_rate]),
                    ])
                  }
                />
              }
            >
              <RichTable
                rows={subs.monthly}
                rowKey={(m) => m.month}
                noun="months"
                pageSize={12}
                columns={[
                  { title: "Month", sort: (m) => m.month, cell: (m) => m.month },
                  { title: "Active at start", sort: (m) => m.active_start, cell: (m) => m.active_start ?? "—", num: true },
                  { title: "New paid", sort: (m) => m.new_subscriptions, cell: (m) => m.new_subscriptions, num: true },
                  { title: "Trials", sort: (m) => m.trials, cell: (m) => m.trials, num: true },
                  { title: "Trial → paid", sort: (m) => m.trial_conversions, cell: (m) => m.trial_conversions, num: true },
                  { title: "Renewals", sort: (m) => m.renewals, cell: (m) => m.renewals, num: true },
                  { title: "Expired", sort: (m) => m.expirations, cell: (m) => m.expirations, num: true },
                  { title: "Churn", sort: (m) => m.churn_rate, cell: (m) => pct(m.churn_rate), num: true },
                  { title: "Renewal rate", sort: (m) => m.renewal_rate, cell: (m) => pct(m.renewal_rate), num: true },
                ]}
              />
            </Section>
          )}

          <Section title="By product" description="* Proceeds: net minus the estimated store fee (Store fee settings).">
            <RichTable
              rows={r.by_product}
              rowKey={(x) => `${x.app_id}:${x.product_id}`}
              noun="products"
              initialSort={{ id: "Net", desc: true }}
              empty="No sales in this period."
              columns={[
                { title: "App", sort: (x) => x.app_id, cell: (x) => x.app_id },
                { title: "Product", sort: (x) => x.product_id, cell: (x) => <Mono>{x.product_id}</Mono> },
                ...moneyColumns<(typeof r.by_product)[number]>(),
              ]}
            />
          </Section>
          <div className="grid gap-6 xl:grid-cols-2">
            <Section title="By app">
              <DataTable
                rows={r.by_app}
                empty="No sales in this period."
                columns={[
                  { title: "App", cell: (x) => x.app_id },
                  ...moneyColumns().slice(0, 4),
                  { title: "Fee", cell: (x) => `${x.commission_percent}%`, num: true },
                ]}
              />
            </Section>
            <Section
              title="By currency"
              description={`ECB rates as of ${r.fx.as_of || "—"}.`}
              actions={<Button size="sm" variant="outline" onClick={() => setFxDialog("")}><PlusIcon /> Manual rate</Button>}
            >
              <DataTable
                rows={r.by_currency}
                empty="No sales in this period."
                columns={[
                  { title: "Currency", cell: (x) => x.currency || "(no price)" },
                  { title: "Gross", cell: (x) => money(x.gross_micros, x.currency), num: true },
                  { title: "Refunds", cell: (x) => money(x.refunded_micros, x.currency), num: true },
                  {
                    title: "Gross, USD",
                    num: true,
                    cell: (x) => (x.converted ? usd(x.gross_usd_micros) : <Button size="xs" variant="outline" onClick={() => setFxDialog(x.currency)}>Set rate</Button>),
                  },
                ]}
              />
              {Object.keys(r.fx.overrides).length > 0 && (
                <div className="mt-4 flex flex-wrap items-center gap-2 text-sm">
                  <span className="text-muted-foreground">Manual rates:</span>
                  {Object.entries(r.fx.overrides).map(([c, v]) => (
                    <span key={c} className="inline-flex items-center gap-1 rounded-md border px-2 py-0.5 font-mono text-xs">
                      1 USD = {v} {c}
                      <button className="text-muted-foreground hover:text-foreground" aria-label={`Remove ${c}`}
                        onClick={() => tryAct(() => call("admin_delete_fx_override", c), "Removed").then(reload)}>
                        <XIcon className="size-3" />
                      </button>
                    </span>
                  ))}
                </div>
              )}
            </Section>
          </div>

          <Section
            title="By country"
            description="Billing country reported by the store (Google Play region, App Store storefront). Purchases recorded before this version have no country."
            actions={
              <CsvButton
                onClick={() =>
                  downloadCsv(`revenue_by_country_${r.from}_${r.to}.csv`, [
                    ["country", "gross_usd_micros", "refunded_usd_micros", "net_usd_micros", "proceeds_usd_micros", "payments", "trials", "refunds"],
                    ...r.by_country.map((c) => [c.country, c.gross_usd_micros, c.refunded_usd_micros, c.net_usd_micros, c.proceeds_usd_micros, payments(c), c.trials, c.refunds]),
                  ])
                }
              />
            }
          >
            <RichTable
              rows={r.by_country}
              rowKey={(c) => c.country || "-"}
              noun="countries"
              initialSort={{ id: "Net", desc: true }}
              empty="No sales in this period."
              columns={[
                { title: "Country", sort: (c) => countryName(c.country), cell: (c) => <span>{countryName(c.country)} {c.country && <Mono className="text-muted-foreground">{c.country}</Mono>}</span> },
                ...moneyColumns<(typeof r.by_country)[number]>().slice(0, 4),
                { title: "Payments", sort: (c) => payments(c), cell: (c) => payments(c), num: true },
                { title: "Trials", sort: (c) => c.trials, cell: (c) => c.trials, num: true },
              ]}
            />
          </Section>

          <CohortSection cohorts={r.cohorts} from={r.from} to={r.to} />

          <Section
            title="By Apple Ads campaign"
            description="Payments in this period by users whose install the Apple Ads attribution of the Unity package resolved to a campaign (asa_campaign_id)."
            actions={
              r.by_asa_campaign.length > 0 && (
                <CsvButton
                  onClick={() =>
                    downloadCsv(`apple_ads_${r.from}_${r.to}.csv`, [
                      ["campaign_id", "payers", "payments", "trials", "refunds", "gross_usd_micros", "refunded_usd_micros", "net_usd_micros"],
                      ...r.by_asa_campaign.map((c) => [c.campaign_id, c.payers, c.payments, c.trials, c.refunds, c.gross_usd_micros, c.refunded_usd_micros, c.net_usd_micros]),
                    ])
                  }
                />
              )
            }
          >
            <RichTable
              rows={r.by_asa_campaign}
              rowKey={(c) => c.campaign_id}
              noun="campaigns"
              initialSort={{ id: "Net", desc: true }}
              empty="No purchases from users attributed to an Apple Ads campaign in this period."
              columns={[
                { title: "Campaign", sort: (c) => c.campaign_id, cell: (c) => <Mono>{c.campaign_id}</Mono> },
                { title: "Payers", sort: (c) => c.payers, cell: (c) => c.payers, num: true },
                { title: "Payments", sort: (c) => c.payments, cell: (c) => c.payments, num: true },
                { title: "Trials", sort: (c) => c.trials, cell: (c) => c.trials, num: true },
                { title: "Gross", sort: (c) => c.gross_usd_micros, cell: (c) => usd(c.gross_usd_micros), num: true },
                { title: "Refunds", sort: (c) => c.refunded_usd_micros, cell: (c) => usd(c.refunded_usd_micros), num: true },
                { title: "Net", sort: (c) => c.net_usd_micros, cell: (c) => <span className="font-medium">{usd(c.net_usd_micros)}</span>, num: true },
                { title: "Net per payer", sort: (c) => (c.payers ? c.net_usd_micros / c.payers : 0), cell: (c) => usd(c.payers ? c.net_usd_micros / c.payers : 0), num: true },
              ]}
            />
          </Section>
        </>
      )}
      <FxDialog currency={fxDialog} onClose={() => setFxDialog(null)} onSaved={reload} />
      {r && feeDialog && <FeeDialog stats={r} onClose={() => setFeeDialog(false)} onSaved={reload} />}
    </>
  )
}

/** Cohort table: cumulative net revenue per payer by month since the first payment. */
function CohortSection({ cohorts, from, to }: { cohorts: Cohort[]; from: string; to: string }) {
  const months = Math.max(0, ...cohorts.map((c) => c.per_payer_usd_micros.length))
  const max = Math.max(1, ...cohorts.flatMap((c) => c.per_payer_usd_micros))
  return (
    <Section
      title="Payer cohorts"
      description="Payers grouped by the month of their first payment; each cell is the cohort's cumulative net revenue per payer by month since then (M0 = the first month). Refunded purchases count as 0."
      actions={
        cohorts.length > 0 && (
          <CsvButton
            onClick={() =>
              downloadCsv(`cohorts_${from}_${to}.csv`, [
                ["cohort", "payers", ...Array.from({ length: months }, (_, i) => `m${i}_per_payer_usd_micros`)],
                ...cohorts.map((c) => [c.cohort, c.payers, ...Array.from({ length: months }, (_, i) => c.per_payer_usd_micros[i] ?? null)]),
              ])
            }
          />
        )
      }
    >
      {cohorts.length === 0 ? (
        <div className="py-8 text-center text-sm text-muted-foreground">No first payments in this period.</div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-separate border-spacing-0.5 text-xs tabular-nums">
            <thead>
              <tr className="text-muted-foreground">
                <th className="px-2 py-1 text-left font-medium">Cohort</th>
                <th className="px-2 py-1 text-right font-medium">Payers</th>
                {Array.from({ length: months }, (_, i) => (
                  <th key={i} className="px-2 py-1 text-right font-medium">M{i}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {cohorts.map((c) => (
                <tr key={c.cohort}>
                  <td className="px-2 py-1 font-medium whitespace-nowrap">{c.cohort}</td>
                  <td className="px-2 py-1 text-right">{c.payers}</td>
                  {Array.from({ length: months }, (_, i) => {
                    const v = c.per_payer_usd_micros[i]
                    if (v === undefined) return <td key={i} />
                    return (
                      <td
                        key={i}
                        className="relative rounded-sm px-2 py-1 text-right whitespace-nowrap"
                        title={`${c.cohort} · M${i}: ${usd(c.revenue_usd_micros[i])} total from ${c.payers} payers`}
                      >
                        <span className="absolute inset-0 rounded-sm bg-[var(--chart-2)]" style={{ opacity: 0.08 + 0.5 * (v / max) }} aria-hidden />
                        <span className="relative">{usd(v)}</span>
                      </td>
                    )
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  )
}

/** Store commission used for the proceeds estimate (server settings `store_commission_*`). */
function FeeDialog({ stats, onClose, onSaved }: { stats: Stats; onClose: () => void; onSaved: () => void }) {
  const [percent, setPercent] = useState(String(stats.proceeds.percent))
  const [afterYear, setAfterYear] = useState(String(stats.proceeds.after_year_percent))
  const [byApp, setByApp] = useState<Record<string, string>>(() =>
    Object.fromEntries(Object.entries(stats.proceeds.by_app).map(([a, v]) => [a, String(v)])),
  )
  const apps = [...new Set([...stats.by_app.map((a) => a.app_id), ...Object.keys(stats.proceeds.by_app)])].sort()

  const save = async () => {
    const num = (s: string) => Number(s.replace(",", "."))
    const overrides = Object.fromEntries(Object.entries(byApp).filter(([, v]) => v.trim() !== "").map(([a, v]) => [a, num(v)]))
    const ok = await tryAct(async () => {
      await call("admin_set_setting", "store_commission_percent", String(num(percent)))
      await call("admin_set_setting", "store_commission_after_year_percent", String(num(afterYear)))
      await call("admin_set_setting", "store_commission_by_app", Object.keys(overrides).length ? JSON.stringify(overrides) : "")
      return true
    }, "Store fee saved")
    if (ok !== undefined) {
      onClose()
      onSaved()
    }
  }

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Store fee</DialogTitle>
          <DialogDescription>
            Proceeds are estimated as net revenue minus this commission. 15% is the rate of the small business programs (Google Play, App
            Store), 30% the standard rate. Taxes are not modelled.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-4">
            <Field label="Default commission, %" hint="Used for every app without its own rate.">
              <div className="flex gap-2">
                <Input value={percent} onChange={(e) => setPercent(e.target.value)} inputMode="decimal" className="w-24" />
                {["15", "30"].map((v) => (
                  <Button key={v} type="button" size="sm" variant={percent === v ? "secondary" : "outline"} onClick={() => setPercent(v)}>{v}%</Button>
                ))}
              </div>
            </Field>
            <Field label="Subscriptions after 12 paid months, %" hint="Both stores charge 15% on a subscription after its first paid year. Applied when lower than the app's rate.">
              <Input value={afterYear} onChange={(e) => setAfterYear(e.target.value)} inputMode="decimal" className="w-24" />
            </Field>
            {apps.length > 0 && (
              <div className="grid gap-2">
                <Label className="text-xs text-muted-foreground">Per build (empty: default)</Label>
                {apps.map((a) => (
                  <div key={a} className="flex items-center gap-2">
                    <Mono className="min-w-0 flex-1 truncate">{a}</Mono>
                    <Input
                      value={byApp[a] ?? ""}
                      onChange={(e) => setByApp({ ...byApp, [a]: e.target.value })}
                      placeholder={percent}
                      inputMode="decimal"
                      className="w-24 shrink-0"
                    />
                  </div>
                ))}
              </div>
            )}
          </div>
        </DialogBody>
        <DialogFooter>
          <Button onClick={save}>Save</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function FxDialog({ currency, onClose, onSaved }: { currency: string | null; onClose: () => void; onSaved: () => void }) {
  const [ccy, setCcy] = useState("")
  const [rate, setRate] = useState("")
  const open = currency !== null
  const save = async () => {
    const value = Number(rate.replace(",", "."))
    const code = (currency || ccy).trim().toUpperCase()
    if (!code || !(value > 0)) return
    const ok = await tryAct(() => call("admin_set_fx_override", code, value), "Rate saved")
    if (ok !== undefined) {
      setRate("")
      setCcy("")
      onClose()
      onSaved()
    }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Manual exchange rate</DialogTitle>
          <DialogDescription>For currencies the ECB does not publish. Used instead of the ECB rate for every day.</DialogDescription>
        </DialogHeader>
        <div className="grid gap-4">
          {!currency && (
            <Field label="Currency (ISO code)">
              <Input value={ccy} onChange={(e) => setCcy(e.target.value)} placeholder="UAH" />
            </Field>
          )}
          <Field label={`How many ${currency || ccy || "units"} is 1 USD?`}>
            <Input value={rate} onChange={(e) => setRate(e.target.value)} placeholder="41.5" inputMode="decimal" autoFocus />
          </Field>
        </div>
        <DialogFooter>
          <Button onClick={save}>Save</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
