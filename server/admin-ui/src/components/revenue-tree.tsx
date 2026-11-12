import { useLayoutEffect, useRef, useState, type ReactNode } from "react"
import {
  CircleDollarSignIcon,
  EyeIcon,
  HourglassIcon,
  RefreshCwIcon,
  RepeatIcon,
  ShoppingBagIcon,
  SparklesIcon,
  TargetIcon,
  UndoIcon,
  UserMinusIcon,
  UsersIcon,
  WalletIcon,
  type LucideIcon,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { ErrorBox, Loading, Section } from "@/components/common"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { pct, usd } from "@/lib/format"
import { useAsync } from "@/lib/hooks"
import { revenueStats } from "@/lib/revenue"
import type { Money } from "@/lib/types"

/** `admin_revenue_stats` with `{"summary": true}`: the parts the tree reads. */
interface TreeSummary {
  totals: Money & { trials: number; trial_conversions: number; expirations: number }
  by_product: (Money & { kind: string })[]
  subscriptions: { series: { day: string; active: number; mrr_usd_micros: number }[]; churn_rate: number | null }
  paywall_views: number
}

/** The numbers of one period. */
interface Metrics {
  views: number
  purchases: number
  trials: number
  newSubs: number
  fromTrials: number
  renewals: number
  expirations: number
  oneTime: number
  subscription: number
  gross: number
  refunded: number
  refunds: number
  net: number
  proceeds: number
  active: number | null
  mrr: number | null
  churn: number | null
}

const isSubscription = (p: TreeSummary["by_product"][number]) =>
  p.kind === "subscription" || (!p.kind && p.new_subscriptions + p.renewals > 0)

function metrics(s: TreeSummary): Metrics {
  const t = s.totals
  const subRevenue = s.by_product.filter(isSubscription).reduce((a, p) => a + p.gross_usd_micros, 0)
  const last = s.subscriptions.series.at(-1)
  return {
    views: s.paywall_views,
    purchases: t.purchases,
    trials: t.trials,
    // The first paid period after a trial is counted as a renewal by the server.
    newSubs: t.new_subscriptions + t.trial_conversions,
    fromTrials: t.trial_conversions,
    renewals: Math.max(0, t.renewals - t.trial_conversions),
    expirations: t.expirations,
    oneTime: t.gross_usd_micros - subRevenue,
    subscription: subRevenue,
    gross: t.gross_usd_micros,
    refunded: t.refunded_usd_micros,
    refunds: t.refunds,
    net: t.net_usd_micros,
    proceeds: t.proceeds_usd_micros,
    active: last ? last.active : null,
    mrr: last ? last.mrr_usd_micros : null,
    churn: s.subscriptions.churn_rate,
  }
}

const isoDay = (d: Date) => d.toISOString().slice(0, 10)
const DAY = 86_400_000
const share = (part: number, whole: number) => (whole > 0 ? part / whole : null)
/** An edge label: nothing when the ratio has no base. */
const rate = (v: number | null, digits = 1) => (v === null ? null : pct(v, digits))
/** Purchases per paywall view; above 100% most purchases happen outside paywalls, so no label. */
const perView = (n: number, views: number) => (views > 0 && n <= views ? pct(n / views) : null)
const count = (n: number) => n.toLocaleString()
const compactUsd = (micros: number) =>
  "$" + (micros / 1e6).toLocaleString(undefined, { notation: micros >= 1e10 ? "compact" : "standard", maximumFractionDigits: micros >= 1e10 ? 1 : 2 })
const wholeUsd = (micros: number) => (micros < 0 ? "−$" : "$") + Math.round(Math.abs(micros) / 1e6).toLocaleString()

type NodeId =
  | "views" | "trials" | "purchases" | "newSubs" | "renewals" | "expirations"
  | "oneTime" | "subscription" | "active" | "gross" | "refunds" | "net"

interface NodeDef {
  id: NodeId
  /** Column (1–5) and row (in steps of 0.5) of the layout; `compactRow`: the row when the
   * subscription nodes are hidden. Empty columns are dropped. */
  col: number
  row: number
  compactRow?: number
  title: string
  role: string
  icon: LucideIcon
  value: (m: Metrics) => number | null
  format: (v: number) => string
  /** Lower is better (refunds, expirations). */
  lowerIsBetter?: boolean
  detail?: (m: Metrics) => string
  help: string
  subscriptions?: boolean
  paywalls?: boolean
  northStar?: boolean
}

const NODES: NodeDef[] = [
  { id: "views", col: 1, row: 1, title: "Paywall views", role: "Driver", icon: EyeIcon, value: (m) => m.views, format: count, paywalls: true,
    detail: () => "from /paywall/event",
    help: "Views reported by the game (POST /paywall/event) for the placements of these apps. Not split by sandbox." },
  { id: "trials", col: 1, row: 3, title: "Trials started", role: "Driver", icon: HourglassIcon, value: (m) => m.trials, format: count, subscriptions: true,
    help: "Free trials started in the period." },
  { id: "purchases", col: 2, row: 1, title: "Purchases", role: "Input", icon: ShoppingBagIcon, value: (m) => m.purchases, format: count,
    detail: () => "one-time",
    help: "Paid consumable and non-consumable purchases." },
  { id: "newSubs", col: 2, row: 3, title: "New subs", role: "Input", icon: SparklesIcon, value: (m) => m.newSubs, format: count, subscriptions: true,
    detail: (m) => `${count(m.newSubs - m.fromTrials)} direct · ${count(m.fromTrials)} from trials`,
    help: "First paid periods: subscriptions bought without a trial plus trials that turned into a paid period." },
  { id: "renewals", col: 2, row: 4, title: "Renewals", role: "Input", icon: RepeatIcon, value: (m) => m.renewals, format: count, subscriptions: true,
    help: "Paid periods after the first one." },
  { id: "expirations", col: 2, row: 5, title: "Expirations", role: "Input", icon: UserMinusIcon, value: (m) => m.expirations, format: count, lowerIsBetter: true, subscriptions: true,
    help: "Subscriptions that ended (not renewed) in the period." },
  { id: "oneTime", col: 3, row: 1, title: "One-time sales", role: "Revenue", icon: WalletIcon, value: (m) => m.oneTime, format: usd,
    help: "Gross revenue of consumable and non-consumable products." },
  { id: "subscription", col: 3, row: 3.5, title: "Subscriptions", role: "Revenue", icon: RefreshCwIcon, value: (m) => m.subscription, format: usd, subscriptions: true,
    help: "Gross revenue of subscription products: first paid periods and renewals." },
  { id: "active", col: 3, row: 5, title: "Active subs", role: "Outcome", icon: UsersIcon, value: (m) => m.active, format: count, subscriptions: true,
    detail: (m) => (m.mrr === null ? "no snapshot yet" : `MRR ${usd(m.mrr)}`),
    help: "Active subscriptions at the last daily snapshot of the period, and their monthly recurring revenue." },
  { id: "gross", col: 4, row: 2, compactRow: 1, title: "Gross revenue", role: "Revenue", icon: CircleDollarSignIcon, value: (m) => m.gross, format: usd,
    help: "All payments at store prices, converted to USD at the ECB rate of each day." },
  { id: "refunds", col: 4, row: 4, compactRow: 2, title: "Refunds", role: "Revenue", icon: UndoIcon, value: (m) => m.refunded, format: usd, lowerIsBetter: true,
    detail: (m) => `${count(m.refunds)} refunded`,
    help: "Money the stores gave back for purchases of the period's days." },
  { id: "net", col: 5, row: 3, compactRow: 1.5, title: "Net revenue", role: "North Star", icon: TargetIcon, value: (m) => m.net, format: usd, northStar: true,
    detail: (m) => `≈ ${wholeUsd(m.proceeds)} after store fee`,
    help: "Gross revenue minus refunds, before the store's commission. Proceeds: minus the estimated store fee (Store fee settings)." },
]

interface EdgeDef {
  from: NodeId
  to: NodeId
  label?: (m: Metrics) => string | null
  /** What the label means (tooltip). */
  help?: string
  negative?: boolean
}

const EDGES: EdgeDef[] = [
  { from: "views", to: "purchases", label: (m) => perView(m.purchases, m.views), help: "Purchases per paywall view (all purchases, also those made outside a paywall)" },
  { from: "views", to: "newSubs", label: (m) => perView(m.newSubs, m.views), help: "New subscriptions per paywall view" },
  { from: "trials", to: "newSubs", label: (m) => rate(share(m.fromTrials, m.trials)), help: "Trials that became paid ÷ trials started in the period" },
  { from: "purchases", to: "oneTime", label: (m) => (m.purchases ? `avg ${compactUsd(m.oneTime / m.purchases)}` : null), help: "Average one-time purchase" },
  { from: "newSubs", to: "subscription" },
  { from: "renewals", to: "subscription", label: (m) => rate(share(m.renewals, m.renewals + m.expirations), 0), help: "Renewal rate: renewals ÷ (renewals + expirations)" },
  { from: "expirations", to: "active", label: (m) => (m.churn === null ? null : `churn ${pct(m.churn)}`), help: "Expirations per 30 days ÷ active at the start of the period", negative: true },
  { from: "oneTime", to: "gross", label: (m) => rate(share(m.oneTime, m.gross), 0), help: "Share of gross revenue" },
  { from: "subscription", to: "gross", label: (m) => rate(share(m.subscription, m.gross), 0), help: "Share of gross revenue" },
  { from: "gross", to: "net" },
  { from: "refunds", to: "net", label: (m) => (m.gross ? `−${pct(share(m.refunded, m.gross))}` : null), help: "Refunds ÷ gross revenue", negative: true },
]

/** Half-row height of the layout grid; a node takes two half-rows. */
const HALF_ROW = 56

/**
 * Where net revenue comes from: paywall views, trials and purchases on the left, revenue in the
 * middle, net revenue on the right; every number with its change against the previous period of
 * the same length. Reads two summary reports (daily aggregates only).
 */
export function RevenueTree({ days, gameKey, sandbox, refreshKey }: { days: number; gameKey: string; sandbox: boolean; refreshKey: unknown }) {
  const { data, error, loading } = useAsync(async () => {
    const to = new Date()
    const from = new Date(to.getTime() - (days - 1) * DAY)
    const q = (f: Date, t: Date) => ({ from: isoDay(f), to: isoDay(t), include_sandbox: sandbox, game_key: gameKey, summary: true })
    const [cur, prev] = await Promise.all([
      revenueStats(q(from, to)),
      revenueStats(q(new Date(from.getTime() - days * DAY), new Date(from.getTime() - DAY))),
    ])
    return { cur: metrics(cur as unknown as TreeSummary), prev: metrics(prev as unknown as TreeSummary) }
  }, [days, gameKey, sandbox, refreshKey])

  return (
    <Section
      title="Revenue tree"
      description={`Where net revenue comes from. Changes compare with the previous ${days} days.`}
    >
      {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={4} /> : <Tree cur={data.cur} prev={data.prev} />}
    </Section>
  )
}

function Tree({ cur, prev }: { cur: Metrics; prev: Metrics }) {
  const hasSubs = [cur, prev].some((m) => m.trials + m.newSubs + m.renewals + m.expirations + m.subscription > 0 || (m.active ?? 0) > 0)
  const hasViews = cur.views + prev.views > 0
  const nodes = NODES.filter((n) => (!n.subscriptions || hasSubs) && (!n.paywalls || hasViews))
  const shown = new Set(nodes.map((n) => n.id))
  const edges = EDGES.filter((e) => shown.has(e.from) && shown.has(e.to))
  const rowOf = (n: NodeDef) => (hasSubs ? n.row : (n.compactRow ?? n.row))
  const cols = [...new Set(nodes.map((n) => n.col))].sort((a, b) => a - b)
  const halfRows = Math.max(...nodes.map(rowOf)) * 2

  const box = useRef<HTMLDivElement>(null)
  const refs = useRef(new Map<NodeId, HTMLDivElement>())
  const [rects, setRects] = useState<Map<NodeId, DOMRect> | null>(null)
  const [origin, setOrigin] = useState({ x: 0, y: 0 })

  useLayoutEffect(() => {
    const el = box.current
    if (!el) return
    const measure = () => {
      const o = el.getBoundingClientRect()
      setOrigin({ x: o.left, y: o.top })
      setRects(new Map([...refs.current].map(([id, n]) => [id, n.getBoundingClientRect()])))
    }
    measure()
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    return () => ro.disconnect()
  }, [nodes.length])

  const paths = rects
    ? edges.flatMap((e) => {
        const a = rects.get(e.from)
        const b = rects.get(e.to)
        if (!a || !b) return []
        const sx = a.right - origin.x
        const sy = a.top + a.height / 2 - origin.y
        const tx = b.left - origin.x
        const ty = b.top + b.height / 2 - origin.y
        const xm = (sx + tx) / 2
        const dy = ty - sy
        const r = Math.min(8, Math.abs(dy) / 2)
        const dir = Math.sign(dy)
        const d =
          Math.abs(dy) < 1
            ? `M${sx},${sy} H${tx}`
            : `M${sx},${sy} H${xm - r} Q${xm},${sy} ${xm},${sy + dir * r} V${ty - dir * r} Q${xm},${ty} ${xm + r},${ty} H${tx}`
        return [{ e, d, sx, sy, label: e.label?.(cur) ?? null, x: xm, y: sy + dy / 2 }]
      })
    : []

  return (
    <div className="-mx-2 overflow-x-auto px-2 pb-1">
      <div
        ref={box}
        className="relative isolate grid gap-x-16"
        style={{
          gridTemplateColumns: `repeat(${cols.length}, minmax(0, 1fr))`,
          gridTemplateRows: `repeat(${halfRows}, ${HALF_ROW}px)`,
          minWidth: cols.length * 168 + (cols.length - 1) * 64,
        }}
      >
        <svg className="pointer-events-none absolute inset-0 size-full overflow-visible" aria-hidden>
          {paths.map(({ e, d }) => (
            <path key={`${e.from}-${e.to}`} d={d} fill="none" className="stroke-border" strokeWidth={1.5} />
          ))}
          {paths.map(({ e, sx, sy }) => (
            <circle key={`dot-${e.from}-${e.to}`} cx={sx} cy={sy} r={2.5} className="fill-muted-foreground/70" />
          ))}
        </svg>
        {nodes.map((n) => (
          <div
            key={n.id}
            ref={(el) => {
              if (el) refs.current.set(n.id, el)
              else refs.current.delete(n.id)
            }}
            className="relative z-10 self-center"
            style={{ gridColumn: cols.indexOf(n.col) + 1, gridRow: `${rowOf(n) * 2 - 1} / span 2` }}
          >
            <TreeNode def={n} cur={cur} prev={prev} />
          </div>
        ))}
        {paths.map(({ e, label, x, y }) =>
          label ? (
            <Tooltip key={`label-${e.from}-${e.to}`}>
              <TooltipTrigger asChild>
                <span
                  className={cn(
                    "absolute z-20 -translate-x-1/2 -translate-y-1/2 cursor-default rounded-md border bg-background px-1.5 py-px text-[11px] font-medium whitespace-nowrap tabular-nums",
                    e.negative ? "border-destructive/40 text-destructive" : "text-foreground",
                  )}
                  style={{ left: x, top: y }}
                >
                  {label}
                </span>
              </TooltipTrigger>
              {e.help && <TooltipContent>{e.help}</TooltipContent>}
            </Tooltip>
          ) : null,
        )}
      </div>
    </div>
  )
}

function TreeNode({ def, cur, prev }: { def: NodeDef; cur: Metrics; prev: Metrics }) {
  const v = def.value(cur)
  const p = def.value(prev)
  const Icon = def.icon
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <div
          className={cn(
            "cursor-default rounded-lg border bg-card px-3 py-2.5 shadow-xs",
            def.northStar && "border-amber-500/50 ring-1 ring-amber-500/30",
          )}
        >
          <div className="flex min-w-0 items-center gap-2">
            <span className="flex size-6 shrink-0 items-center justify-center rounded-md border bg-muted/50 text-muted-foreground">
              <Icon className="size-3.5" />
            </span>
            <div className="min-w-0 leading-tight">
              <div className="truncate text-[13px] font-medium">{def.title}</div>
              <div className={cn("text-[11px]", def.northStar ? "text-amber-600 dark:text-amber-400" : "text-muted-foreground")}>{def.role}</div>
            </div>
          </div>
          <div className="mt-2 flex items-baseline justify-between gap-2">
            <span className="truncate text-base font-semibold tabular-nums">{v === null ? "—" : def.format(v)}</span>
            <Change cur={v} prev={p} lowerIsBetter={def.lowerIsBetter} />
          </div>
          <div className="truncate text-[11px] text-muted-foreground">{def.detail?.(cur) ?? " "}</div>
        </div>
      </TooltipTrigger>
      <TooltipContent side="bottom">
        <div className="grid gap-0.5">
          <span>{def.help}</span>
          <span className="opacity-70">Previous period: {p === null ? "—" : def.format(p)}</span>
        </div>
      </TooltipContent>
    </Tooltip>
  )
}

function Change({ cur, prev, lowerIsBetter }: { cur: number | null; prev: number | null; lowerIsBetter?: boolean }): ReactNode {
  if (cur === null || prev === null) return null
  if (prev === 0) {
    return cur === 0 ? <span className="text-xs text-muted-foreground">—</span> : <span className="text-xs text-muted-foreground">new</span>
  }
  const change = (cur - prev) / Math.abs(prev)
  const good = lowerIsBetter ? change < 0 : change > 0
  const flat = Math.abs(change) < 0.0005
  return (
    <span
      className={cn(
        "shrink-0 text-xs font-medium tabular-nums",
        flat ? "text-muted-foreground" : good ? "text-emerald-600 dark:text-emerald-400" : "text-red-600 dark:text-red-400",
      )}
    >
      {change > 0 ? "+" : flat ? "" : "−"}
      {Math.abs(change * 100).toFixed(1)}%
    </span>
  )
}

