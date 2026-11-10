import { useEffect, useState } from "react"
import { LayoutGridIcon, ListIcon, PlusIcon, SearchIcon, SmartphoneIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { RichTable } from "@/components/rich-table"
import { AddGameWizard } from "@/components/add-game-wizard"
import { ErrorBox, LevelBadge, Loading, Mono, PageHeader, Tone } from "@/components/common"
import { sql } from "@/lib/api"
import { useApps } from "@/lib/apps"
import { buildLabel, platformById } from "@/lib/stores"
import { usd } from "@/lib/format"
import { useAsync } from "@/lib/hooks"
import { revenueSummary } from "@/lib/revenue"
import type { Level } from "@/lib/types"
import { cn } from "@/lib/utils"
import { PLATFORM_ICON } from "@/pages/app"
import { appRoute, type PageProps } from "@/routes"

const RANK: Record<Level, number> = { skip: 0, info: 1, ok: 2, warn: 3, fail: 4 }

type Sort = "name" | "revenue" | "attention"
type View = "cards" | "table"

const VIEW_KEY = "purserAppsView"

function loadView(): View {
  try {
    return localStorage.getItem(VIEW_KEY) === "table" ? "table" : "cards"
  } catch {
    return "cards"
  }
}

export function AppsPage({ go }: PageProps) {
  const [wizard, setWizard] = useState(() => location.hash.endsWith("?new"))
  const [query, setQuery] = useState("")
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
  const loaded = useAsync(async ({ fresh }) => {
    const [health, products, integrations, stats] = await Promise.all([
      sql<{ app_id: string; summary: Level }>("SELECT app_id, summary FROM app_check"),
      sql<{ app_id: string; product_id: string }>("SELECT app_id, product_id FROM product"),
      sql<{ app_id: string; kind: string; enabled: boolean }>("SELECT app_id, kind, enabled FROM integration"),
      revenueSummary(fresh).catch(() => null),
    ])
    const summary: Record<string, Level> = {}
    for (const h of health) if (h.summary) summary[h.app_id] = h.summary
    return { summary, products, integrations, stats }
  })
  const data = groups && loaded.data ? { groups, ...loaded.data } : undefined
  const error = appsError || loaded.error
  const loading = !data

  return (
    <>
      <PageHeader
        title="Apps"
        description="An app is one product with an Android and/or iOS build. Its products, delivery, integrations and keys are on its page."
        actions={<Button onClick={() => setWizard(true)}><PlusIcon /> Add an app</Button>}
      />
      {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={4} /> : !data.groups.length ? (
        <Card className="items-center py-12 text-center">
          <CardContent className="space-y-3">
            <p className="text-sm text-muted-foreground">No apps yet. The wizard sets up the store apps, reward delivery, keys and products, then checks everything.</p>
            <Button onClick={() => setWizard(true)}><PlusIcon /> Add an app</Button>
          </CardContent>
        </Card>
      ) : (
        <>
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-56 flex-1 sm:max-w-sm">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input className="pl-8" placeholder="Search by name, package or bundle id" value={query} onChange={(e) => setQuery(e.target.value)} />
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
          <Tabs value={view} onValueChange={(v) => setView(v as View)} className="ms-auto">
            <TabsList>
              <TabsTrigger value="cards" title="Cards"><LayoutGridIcon /> Cards</TabsTrigger>
              <TabsTrigger value="table" title="Table"><ListIcon /> Table</TabsTrigger>
            </TabsList>
          </Tabs>
        </div>
        {(() => {
          const rows = data.groups
            .map((g) => {
              const ids = g.apps.map((a) => a.app_id)
              const worst = ids.map((id) => data.summary[id]).filter(Boolean).sort((a, b) => RANK[b] - RANK[a])[0]
              const net = (data.stats?.by_app || []).filter((x) => ids.includes(x.app_id)).reduce((s, x) => s + x.net_usd_micros, 0)
              const productCount = new Set(data.products.filter((p) => ids.includes(p.app_id)).map((p) => p.product_id)).size
              const kinds = [...new Set(data.integrations.filter((i) => i.enabled && ids.includes(i.app_id)).map((i) => i.kind))]
              return { g, ids, worst, net, productCount, kinds }
            })
            .filter(({ g }) => {
              const q = query.trim().toLowerCase()
              return !q || g.name.toLowerCase().includes(q) || g.key.toLowerCase().includes(q) || g.apps.some((a) => a.app_id.toLowerCase().includes(q))
            })
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
                            <span key={a.app_id} title={a.app_id} className={cn("inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-xs", !a.enabled && "opacity-50")}>
                              <Icon className="size-3 text-muted-foreground" />
                              {buildLabel(a)}
                            </span>
                          )
                        })}
                      </span>
                    ),
                  },
                  { title: "Bundle ids", hidden: true, size: 280, cell: (r) => <span className="flex flex-col">{r.g.apps.map((a) => <Mono key={a.app_id} className="truncate">{a.app_id}</Mono>)}</span> },
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
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {rows.map(({ g, worst, net, productCount, kinds }) => {
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
                        <span className="w-20 shrink-0 truncate text-muted-foreground">{buildLabel(a)}</span>
                        <Mono className="truncate">{a.app_id}</Mono>
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
          )
        })()}
        </>
      )}
      <AddGameWizard open={wizard} onClose={(key, changed) => { setWizard(false); if (key) go(appRoute(key)); else if (changed) loaded.reload() }} />
    </>
  )
}
