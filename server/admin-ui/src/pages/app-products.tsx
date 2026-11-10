import { useCallback, useState } from "react"
import { CopyIcon, DownloadIcon, FileJsonIcon, PlusIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Textarea } from "@/components/ui/textarea"
import { CatalogPicker, type PickedProduct } from "@/components/catalog-picker"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { call, proc, sqlWhereIn } from "@/lib/api"
import { buildLabel, storeOf } from "@/lib/stores"
import { errorText, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { KINDS, type CatalogRow, type GameApp, type Kind, type Product } from "@/lib/types"
import type { TabProps } from "@/pages/app"

interface Row {
  product_id: string
  kind: Kind
  by: Record<string, Product | undefined>
}

export function AppProductsTab({ data }: TabProps) {
  const g = data.group
  const ids = g.apps.map((a) => a.app_id)
  const [editing, setEditing] = useState<Row | "new" | null>(null)
  /** `import:<app_id>` (store catalog import of that build) or `json`. */
  const [dialog, setDialog] = useState<string | null>(null)
  const products = useAsync(async () => {
    const all = await sqlWhereIn<Product>("product", "app_id", ids)
    const map = new Map<string, Row>()
    for (const p of all) {
      const row = map.get(p.product_id) || { product_id: p.product_id, kind: p.kind, by: {} }
      row.by[p.app_id] = p
      map.set(p.product_id, row)
    }
    return [...map.values()].sort((a, b) => a.product_id.localeCompare(b.product_id))
  }, [ids.join(",")])
  const rows = products.data || []
  /** Builds whose store catalog can be imported (Google Play, App Store). */
  const importers = g.apps.filter((a) => storeOf(a).product_import)

  const copyToOther = async (from: GameApp, to: GameApp) => {
    const items = rows.filter((r) => r.by[from.app_id] && !r.by[to.app_id]).map((r) => {
      const p = r.by[from.app_id]!
      return { app_id: to.app_id, product_id: p.product_id, kind: p.kind, default_price_micros: Number(p.default_price_micros), currency: p.currency }
    })
    if (!items.length) return toast.info(`Every ${buildLabel(from)} product already exists on ${buildLabel(to)}`)
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Copied ${items.length} product(s)`)
    if (ok !== undefined) products.reload()
  }

  return (
    <>
      <Section
        title="Products"
        description="Store product ids per build. The default price is used for revenue when the store does not report one (Google one-time products)."
        actions={
          <>
            {importers.map((a) => (
              <Button key={a.app_id} variant="outline" size="sm" onClick={() => setDialog(`import:${a.app_id}`)}><DownloadIcon /> Import from {storeOf(a).name}</Button>
            ))}
            {g.apps.slice(1).map((to) => (
              <Button key={to.app_id} variant="outline" size="sm" onClick={() => copyToOther(g.apps[0], to)}><CopyIcon /> Copy missing to {buildLabel(to)}</Button>
            ))}
            <Button variant="outline" size="sm" onClick={() => setDialog("json")}><FileJsonIcon /> JSON</Button>
            <Button size="sm" onClick={() => setEditing("new")}><PlusIcon /> Add</Button>
          </>
        }
      >
        {products.error ? <ErrorBox error={products.error} /> : products.loading && !products.data ? <Loading /> : (
          <DataTable
            rows={rows}
            onRowClick={setEditing}
            empty="No products yet."
            columns={[
              { title: "Product", cell: (r) => <Mono>{r.product_id}</Mono> },
              { title: "Type", cell: (r) => r.kind },
              ...g.apps.map((a) => ({
                title: buildLabel(a),
                cell: (r: Row) => {
                  const p = r.by[a.app_id]
                  return p ? <span className="tabular-nums">{money(p.default_price_micros, p.currency) === "—" ? <Tone tone="ok">yes</Tone> : money(p.default_price_micros, p.currency)}</span> : <Tone tone="warn">missing</Tone>
                },
              })),
            ]}
          />
        )}
      </Section>
      <ProductDialog editing={editing} apps={g.apps} onClose={() => setEditing(null)} onSaved={products.reload} />
      {importers.map((a) => (
        <StoreImportDialog key={a.app_id} open={dialog === `import:${a.app_id}`} app={a} others={g.apps.filter((o) => o !== a)} onClose={() => setDialog(null)} onSaved={products.reload} />
      ))}
      <JsonDialog open={dialog === "json"} appId={g.apps[0].app_id} onClose={() => setDialog(null)} onSaved={products.reload} />
    </>
  )
}

function ProductDialog({ editing, apps, onClose, onSaved }: { editing: Row | "new" | null; apps: GameApp[]; onClose: () => void; onSaved: () => void }) {
  return (
    <Dialog open={!!editing} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-lg">
        {editing && <ProductForm key={editing === "new" ? "new" : editing.product_id} row={editing === "new" ? null : editing} apps={apps} onClose={onClose} onSaved={onSaved} />}
      </DialogContent>
    </Dialog>
  )
}

function ProductForm({ row, apps, onClose, onSaved }: { row: Row | null; apps: GameApp[]; onClose: () => void; onSaved: () => void }) {
  const some = row ? Object.values(row.by).find(Boolean) : undefined
  const [productId, setProductId] = useState(row?.product_id || "")
  const [kind, setKind] = useState<Kind>(row?.kind || "consumable")
  const [price, setPrice] = useState(some?.default_price_micros ? String(Number(some.default_price_micros) / 1e6) : "")
  const [currency, setCurrency] = useState(some?.currency || "USD")
  const [on, setOn] = useState<Record<string, boolean>>(() => Object.fromEntries(apps.map((a) => [a.app_id, row ? !!row.by[a.app_id] : true])))

  const save = async () => {
    const id = productId.trim()
    if (!id) return toast.error("Enter the product id")
    const removed = apps.filter((a) => row?.by[a.app_id] && !on[a.app_id])
    if (removed.length && !confirm(`Remove ${id} from ${removed.map((a) => buildLabel(a)).join(" and ")}?`)) return
    const items = apps.filter((a) => on[a.app_id]).map((a) => ({
      app_id: a.app_id, product_id: id, kind, default_price_micros: Math.round(Number(price || 0) * 1e6), currency: currency.trim().toUpperCase(),
    }))
    const ok = await tryAct(async () => {
      if (items.length) await call("admin_upsert_products", JSON.stringify(items))
      for (const a of removed) await call("admin_delete_product", a.app_id, id)
      return true
    }, "Saved")
    if (ok) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>{row ? row.product_id : "New product"}</DialogTitle>
        <DialogDescription>The store product id. Usually the same id exists on both platforms.</DialogDescription>
      </DialogHeader>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Product id" className="sm:col-span-2">
          <Input className="font-mono text-xs" value={productId} onChange={(e) => setProductId(e.target.value)} readOnly={!!row} />
        </Field>
        <Field label="Type" className="sm:col-span-2">
          <Select value={kind} onValueChange={(k) => setKind(k as Kind)}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{KINDS.map((k) => <SelectItem key={k} value={k}>{k}</SelectItem>)}</SelectContent>
          </Select>
        </Field>
        <Field label="Default price"><Input type="number" step="0.01" min="0" value={price} onChange={(e) => setPrice(e.target.value)} /></Field>
        <Field label="Currency (ISO)"><Input value={currency} onChange={(e) => setCurrency(e.target.value)} /></Field>
        <div className="flex flex-wrap gap-6 sm:col-span-2">
          {apps.map((a) => (
            <div key={a.app_id} className="flex items-center gap-2">
              <Checkbox id={`on-${a.app_id}`} checked={!!on[a.app_id]} onCheckedChange={(v) => setOn((o) => ({ ...o, [a.app_id]: v === true }))} />
              <Label htmlFor={`on-${a.app_id}`} className="font-normal">{buildLabel(a)}</Label>
            </div>
          ))}
        </div>
      </div>
      <DialogFooter><Button onClick={save}>Save</Button></DialogFooter>
    </>
  )
}

/** What each store's import needs and leaves to the admin. */
const IMPORT_HELP: Record<string, string> = {
  google: 'Pick consumable or non-consumable for one-time products: Google Play does not say. Needs the permission "View app information" for the service account.',
  apple: "Needs an App Store Connect API Team key (App Manager role) in the build's keys: the In-App Purchase key cannot list products. Prices are not imported: the App Store reports the price of every purchase.",
}

function StoreImportDialog({ open, app, others, onClose, onSaved }: { open: boolean; app: GameApp; others: GameApp[]; onClose: () => void; onSaved: () => void }) {
  const [rows, setRows] = useState<CatalogRow[] | null>(null)
  const [missing, setMissing] = useState<string[]>([])
  const [loading, setLoading] = useState(false)
  const [picked, setPicked] = useState<PickedProduct[]>([])
  const [alsoOthers, setAlsoOthers] = useState(true)
  const onPick = useCallback((p: PickedProduct[]) => setPicked(p), [])
  const load = async () => {
    setLoading(true)
    setRows(null)
    try {
      const r = await proc<{ products: CatalogRow[]; missing_in_store: string[] }>("admin_list_store_products", app.app_id)
      setRows(r.products)
      setMissing(r.missing_in_store)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setLoading(false)
    }
  }
  const save = async () => {
    const targets = [app, ...(alsoOthers ? others : [])]
    const items = targets.flatMap((t) => picked.map((p) => ({ app_id: t.app_id, ...p })))
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Saved ${picked.length} product(s)`)
    if (ok !== undefined) { onSaved(); onClose(); setRows(null) }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => { if (!o) onClose(); else if (!rows && !loading) void load() }}>
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Import from {storeOf(app).name}</DialogTitle>
          <DialogDescription>The catalog of {app.app_id}. {IMPORT_HELP[storeOf(app).id] || ""}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          {!rows && !loading && <Button variant="outline" onClick={load}>Load catalog</Button>}
          {loading ? <Loading /> : rows && (
            <div className="space-y-3">
              <CatalogPicker rows={rows} onChange={onPick} />
              {missing.length > 0 && <p className="text-xs text-muted-foreground">Configured here but not in the store: {missing.join(", ")}</p>}
            </div>
          )}
        </DialogBody>
        <DialogFooter className="gap-3 sm:justify-between">
          {others.length > 0 ? (
            <div className="flex items-center gap-2">
              <Checkbox id="also" checked={alsoOthers} onCheckedChange={(v) => setAlsoOthers(v === true)} />
              <Label htmlFor="also" className="font-normal">Also add to {others.map((o) => buildLabel(o)).join(", ")}</Label>
            </div>
          ) : <span />}
          <Button onClick={save} disabled={!rows || !picked.length}>Save selected</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function JsonDialog({ open, appId, onClose, onSaved }: { open: boolean; appId: string; onClose: () => void; onSaved: () => void }) {
  const [text, setText] = useState<string | null>(null)
  const value = text ?? `[\n  {"app_id": "${appId}", "product_id": "gems_100", "kind": "consumable", "default_price_micros": 990000, "currency": "USD"}\n]`
  const save = async () => {
    try { JSON.parse(value) } catch (e) { return toast.error("Invalid JSON: " + errorText(e)) }
    const ok = await tryAct(() => call("admin_upsert_products", value), "Imported")
    if (ok !== undefined) { onSaved(); onClose(); setText(null) }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Import products as JSON</DialogTitle>
          <DialogDescription>An array of <code>{"{app_id, product_id, kind, default_price_micros, currency}"}</code> objects; existing products are updated.</DialogDescription>
        </DialogHeader>
        <Textarea className="min-h-56 font-mono text-xs" value={value} onChange={(e) => setText(e.target.value)} />
        <DialogFooter><Button onClick={save}>Import</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
