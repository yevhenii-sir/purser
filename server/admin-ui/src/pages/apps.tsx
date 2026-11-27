import { useEffect, useMemo, useState } from "react"
import { LayoutGridIcon, ListIcon, PlusIcon, SearchIcon, SmartphoneIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { RichTable } from "@/components/rich-table"
import { AddGameWizard } from "@/components/add-game-wizard"
import { ErrorBox, LevelBadge, Loading, Mono, PageHeader, Tone } from "@/components/common"
import { proc } from "@/lib/api"
import { useApps } from "@/lib/apps"
import { buildLabel, PLATFORM_ICON, platformById } from "@/lib/stores"
import { usd } from "@/lib/format"
import { useAsync } from "@/lib/hooks"
import { revenueOverview, revenueOverviewIfOn } from "@/lib/revenue"
import { RevenueUpdated } from "@/components/revenue-updated"
import type { Level, RevenueOverview } from "@/lib/types"
import { cn } from "@/lib/utils"
import { appRoute, type PageProps } from "@/routes"
import { useAllowed } from "@/lib/roles"

const RANK: Record<Level, number> = { skip: 0, info: 1, ok: 2, warn: 3, fail: 4 }

type Sort = "name" | "revenue" | "attention"
type View = "cards" | "table"

const VIEW_KEY = "purserAppsView"
/** Cards shown at first and added by "Show more". */
const CARDS_PAGE = 30

/** `admin_apps_summary`: one row per build. */
interface BuildSummary {
  app_id: string
  summary: Level | ""
  checked_at_micros: number
  catalog_ids: string[]
  integrations: string[]
}

function loadView(): View {
  try {
    return localStorage.getItem(VIEW_KEY) === "table" ? "table" : "cards"
  } catch {
    return "cards"
  }
}

export function AppsPage({ go }: PageProps) {
  const addApps = useAllowed("configure")
  const [wizard, setWizard] = useState(() => addApps && location.hash.endsWith("?new"))
  const [query, setQuery] = useState("")
  const [shown, setShown] = useState(CARDS_PAGE)
  const [sort, setSort] = useState<Sort>("name")
  const [view, setViewState] = useState<View>(loadView)
  const setView = (v: View) => {
    setViewState(v)
    try { localStorage.setItem(VIEW_KEY, v) } catch { /* private mode: not remembered */ }
  }
  useEffect(() => {
    if (location.hash.endsWith("?new")) history.replaceState(null, "", "#apps")
  }, [])

  const { groups, error: appsError } = useApps()
  /** A report counted again with the refresh button (newer than the one loaded with the page). */
  const [counted, setCounted] = useState<RevenueOverview | null>(null)
  const loaded = useAsync(async () => {
    const [builds, stats] = await Promise.all([
      proc<BuildSummary[]>("admin_apps_summary"),
      revenueOverviewIfOn(),
    ])
    return { builds: new Map(builds.map((b) => [b.app_id, b])), stats: stats === "off" ? null : stats, analyticsOff: stats === "off" }
  })
  const data = useMemo(() => {
    if (!groups || !loaded.data) return undefined
    const loadedStats = loaded.data.stats
    const stats = counted && counted.computed_at_micros > (loadedStats?.computed_at_micros ?? 0) ? counted : loadedStats
    return { groups, ...loaded.data, stats }
  }, [groups, loaded.data, counted])
  const error = appsError || loaded.error
  const loading = !data
  /** Per app: worst check, revenue, products and integrations; built once per load, not per keystroke. */
  const facts = useMemo(() => {
    if (!data) return []
    const net = new Map<string, number>()
    for (const x of data.stats?.by_app || []) net.set(x.app_id, (net.get(x.app_id) || 0) + x.net_usd_micros)
    return data.groups.map((g) => {
      const ids = g.apps.map((a) => a.app_id)
      const builds = ids.map((id) => data.builds.get(id)).filter((b): b is BuildSummary => !!b)
      const worst = builds.map((b) => b.summary).filter((s): s is Level => !!s).sort((a, b) => RANK[b] - RANK[a])[0]
      return {
        g, ids, worst,
        net: ids.reduce((s, id) => s + (net.get(id) || 0), 0),
        productCount: new Set(builds.flatMap((b) => b.catalog_ids)).size,
        kinds: [...new Set(builds.flatMap((b) => b.integrations))],
        search: [g.name, g.key, ...ids].join(" ").toLowerCase(),
      }
    })
  }, [data])

  return (
    <>
      <PageHeader
        title="Apps"
        description="An app is one product with an Android and/or iOS build. Its products, delivery, integrations and keys are on its page."
        actions={addApps && <Button onClick={() => setWizard(true)}><PlusIcon /> Add an app</Button>}
      />
      {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={4} /> : !data.groups.length ? (
        <Card className="items-center py-12 text-center">
          <CardContent className="space-y-3">
            <p className="text-sm text-muted-foreground">
              {addApps ? "No apps yet. The wizard sets up the store apps, reward delivery, keys and products, then checks everything." : "No apps yet."}
            </p>
            {addApps && <Button onClick={() => setWizard(true)}><PlusIcon /> Add an app</Button>}
          </CardContent>
        </Card>
      ) : (
        <>
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-56 flex-1 sm:max-w-sm">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input className="pl-8" placeholder="Search by name, package or bundle id" value={query} onChange={(e) => { setQuery(e.target.value); setShown(CARDS_PAGE) }} />
          </div>
          {view === "cards" && <Select value={sort} onValueChange={(v) => setSort(v as Sort)}>
            <SelectTrigger className="w-48"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="name">Sort by name</SelectItem>
              <SelectItem value="revenue">Sort by revenue</SelectItem>
              <SelectItem value="attention">Problems first</SelectItem>
            </SelectContent>
          </Select>}
          {view === "cards" && <span className="text-sm text-muted-foreground">{data.groups.length} app{data.groups.length === 1 ? "" : "s"}</span>}
          {data.stats?.rebuilding && <span className="text-xs text-muted-foreground">· revenue totals are being rebuilt</span>}
          {data.analyticsOff && <span className="text-xs text-muted-foreground">· revenue analytics are off (Server → Analytics)</span>}
          {data.stats && <RevenueUpdated stats={data.stats} onRefresh={async () => setCounted(await revenueOverview(true))} prefix="· revenue" className="text-xs text-muted-foreground" />}
          <Tabs value={view} onValueChange={(v) => setView(v as View)} className="ms-auto">
            <TabsList>
              <TabsTrigger value="cards" title="Cards"><LayoutGridIcon /> Cards</TabsTrigger>
              <TabsTrigger value="table" title="Table"><ListIcon /> Table</TabsTrigger>
            </TabsList>
          </Tabs>
        </div>
        {(() => {
          const q = query.trim().toLowerCase()
          const rows = facts
            .filter((f) => !q || f.search.includes(q))
            .sort((x, y) =>
              sort === "revenue" ? y.net - x.net
              : sort === "attention" ? (RANK[y.worst] ?? 2.5) - (RANK[x.worst] ?? 2.5) || x.g.name.localeCompare(y.g.name)
              : x.g.name.localeCompare(y.g.name))
          if (view === "table") {
            return (
              <RichTable
                rows={rows}
                rowKey={(r) => r.g.key}
                noun="apps"
                empty="No app matches."
                onRowClick={(r) => go(appRoute(r.g.key))}
                initialSort={{ id: "App" }}
                columns={[
                  { title: "App", size: 200, sort: (r) => r.g.name.toLowerCase(), cell: (r) => <span className="font-medium">{r.g.name}</span> },
                  {
                    title: "Builds",
                    size: 280,
                    cell: (r) => (
                      <span className="flex flex-wrap gap-1">
                        {r.g.apps.map((a) => {
                          const Icon = PLATFORM_ICON[a.platform] || SmartphoneIcon
                          return (
                            <span key={a.app_id} title={a.store_app_id} className={cn("inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-xs", !a.enabled && "opacity-50")}>
                              <Icon className="size-3 text-muted-foreground" />
                              {buildLabel(a)}
                            </span>
                          )
                        })}
                      </span>
                    ),
                  },
                  { title: "Bundle ids", hidden: true, size: 280, cell: (r) => <span className="flex flex-col">{r.g.apps.map((a) => <Mono key={a.app_id} className="truncate">{a.store_app_id}</Mono>)}</span> },
                  {
                    title: "Setup",
                    size: 120,
                    sort: (r) => (r.worst ? RANK[r.worst] : -1),
                    cell: (r) => (r.worst ? <LevelBadge level={r.worst} /> : <Tone tone="muted">not checked</Tone>),
                  },
                  { title: "Net, 30 d", size: 120, num: true, sort: (r) => r.net, cell: (r) => (data.stats ? usd(r.net) : "—") },
                  { title: "Products", size: 100, num: true, sort: (r) => r.productCount, cell: (r) => r.productCount },
                  {
                    title: "Integrations",
                    size: 180,
                    sort: (r) => r.kinds.length,
                    cell: (r) => <span className="truncate text-muted-foreground">{r.kinds.length ? r.kinds.join(", ") : "—"}</span>,
                  },
                  { title: "Platforms", hidden: true, size: 160, sort: (r) => r.g.apps.length, cell: (r) => [...new Set(r.g.apps.map((a) => platformById(a.platform)?.name || a.platform))].join(", ") },
                ]}
              />
            )
          }
          return (
        <>
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {rows.slice(0, shown).map(({ g, worst, net, productCount, kinds }) => {
            return (
              <Card key={g.key} className="cursor-pointer gap-3 transition-colors hover:bg-muted/40" onClick={() => go(appRoute(g.key))}>
                <CardHeader>
                  <div className="flex items-start justify-between gap-2">
                    <CardTitle className="truncate text-base">{g.name}</CardTitle>
                    {worst ? <LevelBadge level={worst} /> : <Tone tone="muted">not checked</Tone>}
                  </div>
                </CardHeader>
                <CardContent className="space-y-3">
                  <div className="space-y-1">
                    {g.apps.map((a) => (
                      <div key={a.app_id} className="flex items-center gap-2 text-sm">
                        <span className="w-36 shrink-0 truncate text-muted-foreground">{buildLabel(a)}</span>
                        <Mono className="truncate">{a.store_app_id}</Mono>
                        {!a.enabled && <Tone tone="bad">disabled</Tone>}
                      </div>
                    ))}
                  </div>
                  <div className="grid grid-cols-3 gap-2 border-t pt-3 text-sm">
                    <div><div className="text-xs text-muted-foreground">Net, 30 d</div><div className="font-medium tabular-nums">{data.stats ? usd(net) : "—"}</div></div>
                    <div><div className="text-xs text-muted-foreground">Products</div><div className="font-medium tabular-nums">{productCount}</div></div>
                    <div className="min-w-0"><div className="text-xs text-muted-foreground">Integrations</div><div className="truncate font-medium">{kinds.length ? kinds.join(", ") : "—"}</div></div>
                  </div>
                </CardContent>
              </Card>
            )
          })}
        </div>
        {rows.length > shown && (
          <div className="flex justify-center">
            <Button variant="outline" onClick={() => setShown((n) => n + CARDS_PAGE)}>Show more ({rows.length - shown} left)</Button>
          </div>
        )}
        </>
          )
        })()}
        </>
      )}
      <AddGameWizard open={wizard} onClose={(key, changed) => { setWizard(false); if (key) go(appRoute(key)); else if (changed) loaded.reload() }} />
    </>
  )
}
