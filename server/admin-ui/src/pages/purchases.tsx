import { useMemo, useState } from "react"
import { GiftIcon, SearchIcon, UserCheckIcon, UserIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { ErrorBox, Field, Loading, Mono, PageHeader, Section, StatusBadge } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc, route, select } from "@/lib/api"
import { useApps } from "@/lib/apps"
import { errorText, fmtMs, fmtTime, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { openPlayer } from "@/lib/player"
import { STATUSES, type GameApp, type Purchase } from "@/lib/types"
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
  const all = useApps().apps
  // The parent passes a new array on every render: memos and loads key on its contents.
  const appKey = appIds?.join(",")
  const apps = useMemo(() => {
    const ids = appKey?.split(",")
    return (all || []).filter((a) => !ids || ids.includes(a.app_id)).sort((a, b) => a.app_id.localeCompare(b.app_id))
  }, [all, appKey])
  const { data, error, loading, reload } = useAsync(async (): Promise<{ rows: Purchase[]; more: boolean }> => {
    const q = filter.q.trim()
    const status = filter.status === ANY ? "" : filter.status
    if (q) {
      // Exact matches on the indexed id columns.
      const lists = await Promise.all(
        ["id", "user_id", "transaction_id", "original_transaction_id", "purchase_token"].map((column) => select<Purchase>({ table: "purchase", where: { column, equals: q } })),
      )
      let rows = [...new Map(lists.flat().map((r) => [r.id, r])).values()]
      if (appIds) rows = rows.filter((r) => appIds.includes(r.app_id))
      if (status) rows = rows.filter((r) => r.status === status)
      return { rows: rows.sort((a, b) => Number(b.updated_at_micros) - Number(a.updated_at_micros)), more: false }
    }
    // Newest first, filtered on the server before the limit.
    const since = (Date.now() - Number(filter.days) * 86_400_000) * 1000
    return proc("admin_list_purchases", JSON.stringify({ since_micros: since, app_ids: appIds || [], status, limit: LIST_LIMIT }))
  }, [filter, appKey])

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
              { title: "App", size: 200, sort: (r) => r.app_id, cell: (r) => r.app_id },
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

function AppPicker({ apps, value, onChange }: { apps: GameApp[]; value: string; onChange: (v: string) => void }) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger className="w-full"><SelectValue placeholder="Pick an app" /></SelectTrigger>
      <SelectContent>{apps.map((a) => <SelectItem key={a.app_id} value={a.app_id}>{a.app_id} ({a.platform})</SelectItem>)}</SelectContent>
    </Select>
  )
}

/** `userId`: the player is fixed (player page). */
export function GrantDialog({ open, apps, onClose, onDone, userId }: { open: boolean; apps: GameApp[]; onClose: () => void; onDone: () => void; userId?: string }) {
  const [v, setV] = useState({ app_id: "", user_id: userId || "", product_id: "", transaction_id: "", expires: "", note: "" })
  const app = v.app_id || apps[0]?.app_id || ""
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
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="App"><AppPicker apps={apps} value={app} onChange={(a) => setV({ ...v, app_id: a })} /></Field>
          {input("user_id", "user_id", { readOnly: !!userId })}
          {input("product_id", "product_id")}
          {input("transaction_id", "transaction_id (optional)", { placeholder: "manual-…" })}
          {input("expires", "Expires (subscriptions)", { type: "datetime-local" })}
          {input("note", "Note")}
        </div>
        <DialogFooter><Button onClick={grant}>Grant</Button></DialogFooter>
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
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="App"><AppPicker apps={apps} value={target} onChange={setApp} /></Field>
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
