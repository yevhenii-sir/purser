import { useMemo, useState } from "react"
import { DownloadIcon, GiftIcon, SearchIcon, UserCheckIcon, UserIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { AppCombobox } from "@/components/app-combobox"
import { ErrorBox, Field, Loading, Mono, PageHeader, Section, StatusBadge } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc, select } from "@/lib/api"
import { useAppIndex, useApps } from "@/lib/apps"
import { errorText, fmtMs, fmtTime, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { openPlayer } from "@/lib/player"
import { STORES } from "@/lib/stores"
import { catalogIdOf, KINDS, STATUSES, type GameApp, type Product, type Purchase } from "@/lib/types"
import { downloadBlob } from "@/lib/utils"
import { playerRoute, type PageProps } from "@/routes"
import { useAllowed } from "@/lib/roles"

const ANY = "__any__"
/** Rows per page of the list; `admin_list_purchases` says when more match and where they start. */
const LIST_LIMIT = 500
/** Rows per request and at most in one CSV export. */
const EXPORT_PAGE = 1000
const EXPORT_MAX = 50_000

/** Subscription views of the list (`subscription` of `admin_list_purchases`). */
const SUBSCRIPTION_VIEWS: [string, string][] = [
  ["active", "Active subscriptions"],
  ["grace", "In a grace period"],
  ["billing_issue", "Payment failed"],
  ["auto_renew_off", "Auto-renew off"],
  ["price_consent", "Price increase not accepted"],
]

type Page = { rows: Purchase[]; more: boolean; next: { until_micros: number; before_id: string } | null }

const CSV_COLUMNS: (keyof Purchase | string)[] = [
  "id", "app_id", "store", "product_id", "kind", "user_id", "status", "reason", "environment", "price_micros", "currency", "country",
  "transaction_id", "original_transaction_id", "created_at_micros", "updated_at_micros", "expires_at_ms", "trial", "auto_renew_off",
  "billing_issue", "grace_until_ms", "refunded_micros", "offer_id", "price_change", "price_change_micros",
]

function toCsv(rows: Purchase[]): string {
  const cell = (v: unknown) => {
    const t = v === null || v === undefined ? "" : String(v)
    return /[",\n]/.test(t) ? `"${t.replace(/"/g, '""')}"` : t
  }
  return [CSV_COLUMNS.join(","), ...rows.map((r) => CSV_COLUMNS.map((c) => cell((r as unknown as Record<string, unknown>)[c])).join(","))].join("\n")
}

/** `appIds`: only purchases of these store apps (the app page); `embedded`: no page header. */
export function PurchasesPage({ go, appIds, embedded }: PageProps & { appIds?: string[]; embedded?: boolean }) {
  const [query, setQuery] = useState("")
  const [filter, setFilter] = useState({ q: "", status: ANY, days: "7", kind: ANY, sub: ANY })
  /** Pages loaded after the first one ("Load more"), for the filter they were loaded with. */
  const [later, setLater] = useState<{ key: string; pages: Page[] }>({ key: "", pages: [] })
  const [exporting, setExporting] = useState(false)
  const [open, setOpen] = useState<string | null>(null)
  const [dialog, setDialog] = useState<"grant" | "entitlements" | null>(null)
  // App filter of the global page (an app key, "" = every app); the app page passes `appIds`.
  const [game, setGame] = useState("")
  const { groups } = useApps()
  const { byKey, groupOf } = useAppIndex()
  // The parent passes a new array on every render: memos and loads key on its contents.
  const appKey = appIds?.join(",")
  // Builds whose purchases are listed: the app page's, the filtered app's, or none (= every build).
  const shownKey = appIds ? appKey : game ? (byKey.get(game)?.apps.map((a) => a.app_id).join(",") ?? game) : undefined
  // Builds offered by the dialogs: the page's builds, app by app (apps by name, then their builds).
  const apps = useMemo(() => {
    const ids = appKey ? new Set(appKey.split(",")) : null
    return (groups || []).flatMap((g) => g.apps).filter((a) => !ids || ids.has(a.app_id))
  }, [groups, appKey])
  const listQuery = () => ({
    since_micros: (Date.now() - Number(filter.days) * 86_400_000) * 1000,
    app_ids: shownKey?.split(",") || [],
    status: filter.status === ANY ? "" : filter.status,
    kind: filter.sub !== ANY ? "subscription" : filter.kind === ANY ? "" : filter.kind,
    subscription: filter.sub === ANY ? "" : filter.sub,
  })
  const filterKey = JSON.stringify([filter, shownKey])
  const { data, error, loading, reload } = useAsync(async (): Promise<Page> => {
    const q = filter.q.trim()
    const status = filter.status === ANY ? "" : filter.status
    const ids = shownKey?.split(",") || []
    if (q) {
      // Exact matches on the indexed id columns. A store's transaction id is the ledger id without
      // its `{store}:` prefix.
      const ledgerIds = [q, ...STORES.map((s) => `${s.id}:${q}`)]
      const lists = await Promise.all([
        select<Purchase>({ table: "purchase", where: { column: "id", equals: ledgerIds } }),
        ...["user_id", "original_transaction_id", "purchase_token"].map((column) => select<Purchase>({ table: "purchase", where: { column, equals: q } })),
      ])
      let rows = [...new Map(lists.flat().map((r) => [r.id, r])).values()]
      if (ids.length) {
        const only = new Set(ids)
        rows = rows.filter((r) => only.has(r.app_id))
      }
      if (status) rows = rows.filter((r) => r.status === status)
      return { rows: rows.sort((a, b) => Number(b.updated_at_micros) - Number(a.updated_at_micros)), more: false, next: null }
    }
    // Newest first, filtered on the server before the limit.
    return proc("admin_list_purchases", JSON.stringify({ ...listQuery(), limit: LIST_LIMIT }))
  }, [filter, shownKey])
  const pages = data ? [data, ...(later.key === filterKey ? later.pages : [])] : []
  const rows = pages.flatMap((p) => p.rows)
  const last = pages.at(-1)
  const loadMore = async () => {
    if (!last?.next) return
    const page = await tryAct(() => proc<Page>("admin_list_purchases", JSON.stringify({ ...listQuery(), ...last.next, limit: LIST_LIMIT })))
    if (page) setLater((l) => ({ key: filterKey, pages: [...(l.key === filterKey ? l.pages : []), page] }))
  }
  const exportCsv = async () => {
    setExporting(true)
    try {
      const out: Purchase[] = []
      let next: Page["next"] = null
      do {
        const page: Page = await proc<Page>("admin_list_purchases", JSON.stringify({ ...listQuery(), ...(next || {}), limit: EXPORT_PAGE }))
        out.push(...page.rows)
        next = page.next
      } while (next && out.length < EXPORT_MAX)
      downloadBlob(`purchases-${new Date().toISOString().slice(0, 10)}.csv`, toCsv(out), "text/csv")
      if (next) toast.warning(`Exported the newest ${out.length} purchases; pick a shorter period for the rest`)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setExporting(false)
    }
  }

  const support = useAllowed("support")
  const actions = (
    <>
      <Button variant="outline" onClick={() => setDialog("entitlements")}><UserCheckIcon /> Entitlements</Button>
      {support && <Button onClick={() => setDialog("grant")}><GiftIcon /> Manual grant</Button>}
    </>
  )
  return (
    <>
      {!embedded && (
        <PageHeader title="Purchases" description="The purchase ledger of every app: search by player, transaction id or purchase token." actions={actions} />
      )}
      <div className="flex flex-wrap gap-2">
        <div className="relative min-w-64 flex-1">
          <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
          <Input className="pl-8" placeholder="user_id, transaction id, purchase token or purchase id" value={query}
            onChange={(e) => setQuery(e.target.value)} onKeyDown={(e) => e.key === "Enter" && setFilter((f) => ({ ...f, q: query }))} />
        </div>
        {!appIds && <AppCombobox mode="app" allowAll value={game} onChange={setGame} className="w-56" />}
        <Select value={filter.status} onValueChange={(status) => setFilter((f) => ({ ...f, status }))}>
          <SelectTrigger className="w-44"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value={ANY}>All statuses</SelectItem>
            {STATUSES.map((s) => <SelectItem key={s} value={s}>{s}</SelectItem>)}
          </SelectContent>
        </Select>
        <Select value={filter.sub !== ANY ? "subscription" : filter.kind} onValueChange={(kind) => setFilter((f) => ({ ...f, kind, sub: ANY }))} disabled={!!filter.q}>
          <SelectTrigger className="w-40"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value={ANY}>All kinds</SelectItem>
            {KINDS.map((k) => <SelectItem key={k} value={k}>{k.replace("_", "-")}</SelectItem>)}
          </SelectContent>
        </Select>
        <Select value={filter.sub} onValueChange={(sub) => setFilter((f) => ({ ...f, sub }))} disabled={!!filter.q}>
          <SelectTrigger className="w-48"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value={ANY}>Any state</SelectItem>
            {SUBSCRIPTION_VIEWS.map(([v, l]) => <SelectItem key={v} value={v}>{l}</SelectItem>)}
          </SelectContent>
        </Select>
        <Select value={filter.days} onValueChange={(days) => setFilter((f) => ({ ...f, days }))} disabled={!!filter.q}>
          <SelectTrigger className="w-36"><SelectValue /></SelectTrigger>
          <SelectContent>{["1", "7", "30", "365"].map((d) => <SelectItem key={d} value={d}>Last {d} d</SelectItem>)}</SelectContent>
        </Select>
        <Button variant="outline" onClick={() => setFilter((f) => ({ ...f, q: query }))}>Search</Button>
        <Button variant="outline" disabled={exporting || !!filter.q} onClick={exportCsv} title="All purchases of the filter as CSV"><DownloadIcon /> {exporting ? "Exporting…" : "CSV"}</Button>
        <Button variant="outline" disabled={!query.trim()} onClick={() => openPlayer(query, go)} title="Open the player this user_id, transaction id or purchase token belongs to">
          <UserIcon /> Open player
        </Button>
        {embedded && <div className="ms-auto flex flex-wrap gap-2">{actions}</div>}
      </div>
      <Section>
        {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={6} /> : (
          <RichTable
            rows={rows}
            onRowClick={(r) => setOpen(r.id)}
            rowKey={(r) => r.id}
            // The server stops at its scan limit: an empty page with more to read is not "nothing".
            empty={last?.next ? "No matches in the purchases read so far: load more to search further back." : "Nothing found."}
            noun="purchases"
            summary={last?.more ? `The newest ${rows.length} purchases of the period and filters` : undefined}
            initialSort={{ id: "Updated", desc: true }}
            columns={[
              { title: "Updated", size: 170, sort: (r) => r.updated_at_micros, cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.updated_at_micros)}</span> },
              { title: "Created", size: 170, sort: (r) => r.created_at_micros, hidden: true, cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.created_at_micros)}</span> },
              {
                title: "App",
                size: 200,
                sort: (r) => groupOf.get(r.app_id)?.name ?? r.app_id,
                cell: (r) => {
                  const g = groupOf.get(r.app_id)
                  return (
                    <span className="block min-w-0 truncate" title={r.app_id}>
                      {g && !appIds ? <>{g.name} <span className="font-mono text-xs text-muted-foreground">{r.app_id}</span></> : r.app_id}
                    </span>
                  )
                },
              },
              { title: "Product", size: 130, sort: (r) => r.product_id, cell: (r) => <Mono>{r.product_id}</Mono> },
              {
                title: "Player",
                size: 130,
                sort: (r) => r.user_id,
                cell: (r) => (
                  <a href={"#" + playerRoute(r.user_id)} className="hover:underline" title="Open the player">
                    <Mono>{String(r.user_id).length > 18 ? String(r.user_id).slice(0, 18) + "…" : r.user_id}</Mono>
                  </a>
                ),
              },
              { title: "Status", size: 130, sort: (r) => r.status, cell: (r) => <StatusBadge status={r.status} /> },
              { title: "Reason", size: 100, sort: (r) => r.reason, cell: (r) => r.reason },
              { title: "Env", size: 110, sort: (r) => r.environment, cell: (r) => r.environment },
              { title: "Price", size: 110, num: true, hidden: true, sort: (r) => r.price_micros, cell: (r) => (r.price_micros ? money(r.price_micros, r.currency) : "—") },
              { title: "Country", size: 90, hidden: true, sort: (r) => String(r.country || ""), cell: (r) => String(r.country || "—") },
              { title: "Transaction", size: 200, hidden: true, sort: (r) => r.transaction_id, cell: (r) => <Mono>{r.transaction_id}</Mono> },
              { title: "Expires", size: 170, sort: (r) => r.expires_at_ms || null, cell: (r) => <span className="whitespace-nowrap">{fmtMs(r.expires_at_ms)}</span> },
            ]}
          />
        )}
        {last?.next && !loading && <div className="mt-3 flex justify-center"><Button variant="outline" onClick={loadMore}>Load {LIST_LIMIT} more</Button></div>}
      </Section>
      <PurchaseSheet id={open} onClose={(changed) => { setOpen(null); if (changed) reload() }} />
      <GrantDialog open={dialog === "grant"} apps={apps} onClose={() => setDialog(null)} onDone={reload} />
      <EntitlementsDialog open={dialog === "entitlements"} apps={apps} onClose={() => setDialog(null)} />
    </>
  )
}

/** `userId`: the player is fixed (player page). */
export function GrantDialog({ open, apps, onClose, onDone, userId }: { open: boolean; apps: GameApp[]; onClose: () => void; onDone: () => void; userId?: string }) {
  const [v, setV] = useState({ app_id: "", user_id: userId || "", product_id: "", transaction_id: "", expires: "", note: "" })
  const app = v.app_id || apps[0]?.app_id || ""
  const products = useAsync(
    async () => (app ? await select<Product>({ table: "product", where: { column: "app_id", equals: app } }) : []).sort((a, b) => a.product_id.localeCompare(b.product_id)),
    [app],
  )
  const product = products.data?.find((p) => p.product_id === v.product_id)
  const subscription = product?.kind === "subscription"
  const grant = async () => {
    const payload = { ...v, user_id: v.user_id.trim(), app_id: app, expires_at_ms: v.expires ? new Date(v.expires).getTime() : 0 }
    const ok = await tryAct(() => call("admin_grant_manual", JSON.stringify(payload)), "Granted: delivered within a minute")
    if (ok !== undefined) { onDone(); onClose() }
  }
  const input = (k: keyof typeof v, label: string, props: Record<string, unknown> = {}) => (
    <Field label={label}><Input value={v[k]} onChange={(e) => setV({ ...v, [k]: e.target.value })} {...props} /></Field>
  )
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>Manual grant</DialogTitle>
          <DialogDescription>Compensation or promo without a purchase: recorded in the ledger and delivered the usual way; not revenue and not sent to analytics.</DialogDescription>
        </DialogHeader>
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label="App"><AppCombobox mode="build" builds={apps} value={app} onChange={(a) => setV({ ...v, app_id: a, product_id: "", expires: "" })} /></Field>
          {input("user_id", "user_id", { readOnly: !!userId })}
          <Field label="Product" hint={products.data?.length === 0 ? "The build has no products: add them on the app's Products tab." : undefined}>
            <Select value={v.product_id} onValueChange={(id) => setV({ ...v, product_id: id })} disabled={!products.data?.length}>
              <SelectTrigger className="w-full"><SelectValue placeholder={products.loading ? "Loading…" : "Pick a product"} /></SelectTrigger>
              <SelectContent>
                {products.data?.map((p) => (
                  <SelectItem key={p.product_id} value={p.product_id}>
                    <span className="flex min-w-0 items-baseline gap-2">
                      <Mono className="truncate">{p.product_id}</Mono>
                      <span className="shrink-0 text-xs text-muted-foreground">{p.kind.replace("_", "-")}{catalogIdOf(p) !== p.product_id ? ` · ${catalogIdOf(p)}` : ""}</span>
                    </span>
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
          {input("transaction_id", "transaction_id (optional)", { placeholder: "manual-…" })}
          {input("expires", "Expires", { type: "datetime-local", disabled: !subscription, title: subscription ? "" : "Only subscriptions expire" })}
          {input("note", "Note")}
        </div>
        <DialogFooter><Button onClick={grant} disabled={!v.product_id || !v.user_id.trim()}>Grant</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function EntitlementsDialog({ open, apps, onClose }: { open: boolean; apps: GameApp[]; onClose: () => void }) {
  const [app, setApp] = useState("")
  const [user, setUser] = useState("")
  const [result, setResult] = useState<unknown>(null)
  const [error, setError] = useState("")
  const target = app || apps[0]?.app_id || ""
  const check = async () => {
    setError("")
    try {
      // Through the admin procedure: client routes of signed builds need a player token.
      const game = apps.find((x) => x.app_id === target)?.game_key
      const profile = await proc<{ entitlements: { game_key: string }[] }>("admin_user_profile", user.trim())
      setResult(profile.entitlements.find((e) => e.game_key === game) ?? { access: {}, subscriptions: {}, non_consumables: [] })
    } catch (e) {
      setResult(null)
      setError(errorText(e))
    }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>Player entitlements</DialogTitle>
          <DialogDescription>What /entitlements returns for a player across the apps of the same game.</DialogDescription>
        </DialogHeader>
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label="App"><AppCombobox mode="build" builds={apps} value={target} onChange={setApp} /></Field>
          <Field label="user_id"><Input value={user} onChange={(e) => setUser(e.target.value)} /></Field>
        </div>
        {error && <ErrorBox error={error} />}
        {result !== null && <pre className="max-h-72 overflow-auto rounded-md bg-muted p-3 font-mono text-xs">{JSON.stringify(result, null, 2)}</pre>}
        <DialogFooter>
          <Button onClick={check} disabled={!user}>Check</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
