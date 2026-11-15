import { useMemo, useState } from "react"
import { GiftIcon, SearchIcon, UserCheckIcon, UserIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { AppCombobox } from "@/components/app-combobox"
import { ErrorBox, Field, Loading, Mono, PageHeader, Section, StatusBadge } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc, route, select } from "@/lib/api"
import { useAppIndex, useApps } from "@/lib/apps"
import { errorText, fmtMs, fmtTime, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { openPlayer } from "@/lib/player"
import { catalogIdOf, STATUSES, type GameApp, type Product, type Purchase } from "@/lib/types"
import { playerRoute, type PageProps } from "@/routes"

const ANY = "__any__"
/** Rows of the list; `admin_list_purchases` says when more match. */
const LIST_LIMIT = 500

/** `appIds`: only purchases of these store apps (the app page); `embedded`: no page header. */
export function PurchasesPage({ go, appIds, embedded }: PageProps & { appIds?: string[]; embedded?: boolean }) {
  const [query, setQuery] = useState("")
  const [filter, setFilter] = useState({ q: "", status: ANY, days: "7" })
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
  const { data, error, loading, reload } = useAsync(async (): Promise<{ rows: Purchase[]; more: boolean }> => {
    const q = filter.q.trim()
    const status = filter.status === ANY ? "" : filter.status
    const ids = shownKey?.split(",") || []
    if (q) {
      // Exact matches on the indexed id columns.
      const lists = await Promise.all(
        ["id", "user_id", "transaction_id", "original_transaction_id", "purchase_token"].map((column) => select<Purchase>({ table: "purchase", where: { column, equals: q } })),
      )
      let rows = [...new Map(lists.flat().map((r) => [r.id, r])).values()]
      if (ids.length) {
        const only = new Set(ids)
        rows = rows.filter((r) => only.has(r.app_id))
      }
      if (status) rows = rows.filter((r) => r.status === status)
      return { rows: rows.sort((a, b) => Number(b.updated_at_micros) - Number(a.updated_at_micros)), more: false }
    }
    // Newest first, filtered on the server before the limit.
    const since = (Date.now() - Number(filter.days) * 86_400_000) * 1000
    return proc("admin_list_purchases", JSON.stringify({ since_micros: since, app_ids: ids, status, limit: LIST_LIMIT }))
  }, [filter, shownKey])

  const actions = (
    <>
      <Button variant="outline" onClick={() => setDialog("entitlements")}><UserCheckIcon /> Entitlements</Button>
      <Button onClick={() => setDialog("grant")}><GiftIcon /> Manual grant</Button>
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
        <Select value={filter.days} onValueChange={(days) => setFilter((f) => ({ ...f, days }))} disabled={!!filter.q}>
          <SelectTrigger className="w-36"><SelectValue /></SelectTrigger>
          <SelectContent>{["1", "7", "30", "365"].map((d) => <SelectItem key={d} value={d}>Last {d} d</SelectItem>)}</SelectContent>
        </Select>
        <Button variant="outline" onClick={() => setFilter((f) => ({ ...f, q: query }))}>Search</Button>
        <Button variant="outline" disabled={!query.trim()} onClick={() => openPlayer(query, go)} title="Open the player this user_id, transaction id or purchase token belongs to">
          <UserIcon /> Open player
        </Button>
        {embedded && <div className="ms-auto flex flex-wrap gap-2">{actions}</div>}
      </div>
      <Section>
        {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={6} /> : (
          <RichTable
            rows={data.rows}
            onRowClick={(r) => setOpen(r.id)}
            rowKey={(r) => r.id}
            empty="Nothing found."
            noun="purchases"
            summary={data.more ? `The newest ${data.rows.length} purchases: pick a shorter period or a status to see the rest` : undefined}
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
    const payload = { ...v, app_id: app, expires_at_ms: v.expires ? new Date(v.expires).getTime() : 0 }
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
      setResult((await route("/entitlements", { app_id: target, user_id: user })).json)
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
