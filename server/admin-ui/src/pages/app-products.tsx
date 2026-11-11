import { useCallback, useState } from "react"
import { ChevronDownIcon, ClipboardCopyIcon, ClipboardPasteIcon, CopyIcon, DownloadIcon, FileJsonIcon, PlusIcon, RefreshCwIcon, StoreIcon, UploadIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Textarea } from "@/components/ui/textarea"
import { CatalogPicker, type PickedProduct } from "@/components/catalog-picker"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { StoreCreateDialog, StoreJobs } from "@/components/store-create-dialog"
import { call, proc, publicRoute, sqlWhereIn } from "@/lib/api"
import { buildLabel, creatorBuilds, storeById, storeOf } from "@/lib/stores"
import { errorText, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { catalogIdOf, KINDS, listingOf, type CatalogRow, type GameApp, type Kind, type Product } from "@/lib/types"
import { PERIODS, perPeriod } from "@/lib/periods"
import type { TabProps } from "@/pages/app"
import { appRoute } from "@/routes"
import { useCan } from "@/lib/roles"
import { parseUnityConfig, productIdIn, toUnityConfig, type UnityBuild } from "@/lib/unity-config"

/** One product of the app: its catalog id and its row in each build (whose `product_id` is the store id). */
interface Row {
  /** The catalog id. */
  product_id: string
  kind: Kind
  by: Record<string, Product | undefined>
}

/** The products of the app's builds grouped by catalog id. */
function groupByCatalogId(all: Product[]): Row[] {
  const map = new Map<string, Row>()
  for (const p of all) {
    const id = catalogIdOf(p)
    const row = map.get(id) || { product_id: id, kind: p.kind, by: {} }
    row.by[p.app_id] = p
    map.set(id, row)
  }
  return [...map.values()].sort((a, b) => a.product_id.localeCompare(b.product_id))
}

/** Store ids per store where a build's product id differs from the catalog id (the Unity config's `storeIds`). */
function storeIdsOf(r: Row, apps: GameApp[]): Record<string, string> {
  const out: Record<string, string> = {}
  for (const a of apps) {
    const p = r.by[a.app_id]
    if (p && p.product_id !== r.product_id && !out[storeOf(a).id]) out[storeOf(a).id] = p.product_id
  }
  return out
}

type Listing = { title: string; description: string; period: string; apple_group: string }

/** The store listing of a product: each field from the first build that has it set. */
function listingOfRow(r: Row): Listing {
  const ps = Object.values(r.by).filter((p): p is Product => !!p)
  const first = (k: keyof Listing) => ps.map((p) => p[k] || "").find(Boolean) || ""
  return { title: first("title"), description: first("description"), period: first("period"), apple_group: first("apple_group") }
}

export function AppProductsTab({ data, go }: TabProps) {
  const g = data.group
  const ids = g.apps.map((a) => a.app_id)
  const [editing, setEditing] = useState<Row | "new" | null>(null)
  /** `import:<app_id>` (store catalog import of that build), `create`, `unity` or `json`. */
  const [dialog, setDialog] = useState<string | null>(null)
  /** Remounts the store job list after new jobs were queued. */
  const [jobsKey, setJobsKey] = useState(0)
  const products = useAsync(async () => groupByCatalogId(await sqlWhereIn<Product>("product", "app_id", ids)), [ids.join(",")])
  const rows = products.data || []
  const configure = useCan()("configure")
  /** Builds whose store catalog can be imported (Google Play, App Store). */
  const importers = g.apps.filter((a) => storeOf(a).product_import)

  const copyToOther = async (from: GameApp, to: GameApp) => {
    const items = rows.filter((r) => r.by[from.app_id] && !r.by[to.app_id]).map((r) => {
      const p = r.by[from.app_id]!
      return {
        app_id: to.app_id, product_id: p.product_id, catalog_id: r.product_id, kind: p.kind, default_price_micros: Number(p.default_price_micros), currency: p.currency,
        ...listingOf(p),
      }
    })
    if (!items.length) return toast.info(`Every ${buildLabel(from)} product already exists on ${buildLabel(to)}`)
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Copied ${items.length} product(s)`)
    if (ok !== undefined) products.reload()
  }

  /** Every build's products as the `admin_upsert_products` JSON (Import → From JSON reads it back). */
  const downloadJson = () => {
    const items = rows.flatMap((r) => g.apps.flatMap((a) => {
      const p = r.by[a.app_id]
      return p ? [{
        app_id: a.app_id, product_id: p.product_id, ...(p.product_id !== r.product_id ? { catalog_id: r.product_id } : {}), kind: p.kind,
        default_price_micros: Number(p.default_price_micros), currency: p.currency, ...listingOf(p),
      }] : []
    }))
    const url = URL.createObjectURL(new Blob([JSON.stringify(items, null, 2) + "\n"], { type: "application/json" }))
    const link = Object.assign(document.createElement("a"), { href: url, download: `products-${g.key}.json` })
    link.click()
    URL.revokeObjectURL(url)
  }

  /** The app's builds and products as the Unity config (inspector: Paste config from admin panel). */
  const copyUnityConfig = async () => {
    const products = rows.map((r) => {
      const priced = g.apps.map((a) => r.by[a.app_id]).find((p) => p && Number(p.default_price_micros) > 0)
      return {
        product_id: r.product_id, kind: r.kind, store_ids: storeIdsOf(r, g.apps),
        default_price_micros: priced ? Number(priced.default_price_micros) : 0, currency: priced?.currency || "", period: listingOfRow(r).period,
      }
    })
    const text = toUnityConfig(await publicRoute(), g.apps.map((a) => ({ store: storeOf(a).id, app_id: a.app_id })), products)
    await tryAct(() => navigator.clipboard.writeText(text), "Copied: in Unity press Paste config from admin panel")
  }

  return (
    <>
      <Section
        title="Products"
        description="The app's products and their ids in each build's store. The default price is used for revenue when the store does not report one (Google one-time products); the title, description and period are used by Create in store."
        actions={
          <>
            {configure && (
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="outline" size="sm"><DownloadIcon /> Import <ChevronDownIcon /></Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="min-w-56">
                  {importers.map((a) => (
                    <DropdownMenuItem key={a.app_id} onClick={() => setDialog(`import:${a.app_id}`)}>From {storeOf(a).name}{importers.length > 1 && storeOf(a).name !== buildLabel(a) ? ` (${buildLabel(a)})` : ""}</DropdownMenuItem>
                  ))}
                  {importers.length > 0 && <DropdownMenuSeparator />}
                  <DropdownMenuItem onClick={() => setDialog("unity")}><ClipboardPasteIcon /> From Unity config</DropdownMenuItem>
                  <DropdownMenuItem onClick={() => setDialog("json")}><FileJsonIcon /> From JSON</DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            )}
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="outline" size="sm" disabled={!products.data}><UploadIcon /> Export <ChevronDownIcon /></Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="min-w-56">
                <DropdownMenuItem onClick={copyUnityConfig}><ClipboardCopyIcon /> Copy Unity config</DropdownMenuItem>
                <DropdownMenuItem onClick={downloadJson}><FileJsonIcon /> Download JSON</DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
            {configure && (g.apps.length > 1 || creatorBuilds(g.apps).length > 0) && (
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="outline" size="sm" disabled={!products.data}><RefreshCwIcon /> Sync <ChevronDownIcon /></Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="min-w-56">
                  {g.apps.slice(1).map((to) => (
                    <DropdownMenuItem key={to.app_id} onClick={() => copyToOther(g.apps[0], to)}><CopyIcon /> Copy missing to {buildLabel(to)}</DropdownMenuItem>
                  ))}
                  {creatorBuilds(g.apps).length > 0 && (
                    <>
                      {g.apps.length > 1 && <DropdownMenuSeparator />}
                      <DropdownMenuItem onClick={() => setDialog("create")}><StoreIcon /> Create in store…</DropdownMenuItem>
                    </>
                  )}
                </DropdownMenuContent>
              </DropdownMenu>
            )}
            {configure && <Button size="sm" onClick={() => setEditing("new")}><PlusIcon /> Add product</Button>}
          </>
        }
      >
        {products.error ? <ErrorBox error={products.error} /> : products.loading && !products.data ? <Loading /> : (
          <DataTable
            rows={rows}
            onRowClick={configure ? setEditing : undefined}
            empty="No products yet."
            columns={[
              { title: "Product", cell: (r) => <Mono>{r.product_id}</Mono> },
              { title: "Type", cell: (r) => r.kind },
              ...g.apps.map((a) => ({
                title: buildLabel(a),
                cell: (r: Row) => {
                  const p = r.by[a.app_id]
                  if (!p) return <Tone tone="warn">missing</Tone>
                  const price = money(p.default_price_micros, p.currency)
                  const per = r.kind === "subscription" ? perPeriod(p.period) : ""
                  const shown = price === "—" ? <Tone tone="ok">{per ? `yes · per ${per}` : "yes"}</Tone> : <span className="tabular-nums">{price}{per && <span className="text-muted-foreground"> / {per}</span>}</span>
                  if (p.product_id === r.product_id) return shown
                  return (
                    <div className="min-w-0">
                      {shown}
                      <div className="max-w-48 truncate font-mono text-xs text-muted-foreground" title={p.product_id}>{p.product_id}</div>
                    </div>
                  )
                },
              })),
            ]}
          />
        )}
      </Section>
      {<StoreJobs key={jobsKey} apps={g.apps} onFinished={products.reload} />}
      <ProductDialog editing={editing} apps={g.apps} onClose={() => setEditing(null)} onSaved={products.reload} />
      <StoreCreateDialog open={dialog === "create"} apps={g.apps} listed={(app, id) => !!rows.find((r) => r.product_id === id)?.by[app]} onClose={() => setDialog(null)} onStarted={() => { products.reload(); setJobsKey((k) => k + 1) }}
        products={rows.map((r) => {
          const priced = g.apps.map((a) => r.by[a.app_id]).find((p) => p && Number(p.default_price_micros) > 0)
          return { product_id: r.product_id, kind: r.kind, price_micros: priced ? Number(priced.default_price_micros) : 0, currency: priced?.currency || "USD", ...listingOfRow(r) }
        })} />
      {importers.map((a) => (
        <StoreImportDialog key={a.app_id} open={dialog === `import:${a.app_id}`} app={a} others={g.apps.filter((o) => o !== a)} onClose={() => setDialog(null)} onSaved={products.reload} />
      ))}
      <UnityImportDialog open={dialog === "unity"} apps={g.apps} existing={products.data ? rows : null} onClose={() => setDialog(null)} onSaved={products.reload}
        onWizard={() => go(appRoute(g.key, "setup"))} />
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
  const [listing, setListing] = useState<Listing>(() => (row ? listingOfRow(row) : { title: "", description: "", period: "", apple_group: "" }))
  const setL = (patch: Partial<Listing>) => setListing((l) => ({ ...l, ...patch }))
  const [on, setOn] = useState<Record<string, boolean>>(() => Object.fromEntries(apps.map((a) => [a.app_id, row ? !!row.by[a.app_id] : true])))
  /** Store id per build where it differs from the product id (Stripe Price ids, reverse-domain App Store ids). */
  const [storeIds, setStoreIds] = useState<Record<string, string>>(() =>
    Object.fromEntries(apps.map((a) => {
      const p = row?.by[a.app_id]
      return [a.app_id, p && p.product_id !== row?.product_id ? p.product_id : ""]
    })))

  const save = async () => {
    const id = productId.trim()
    if (!id) return toast.error("Enter the product id")
    const storeIdOf = (a: GameApp) => storeIds[a.app_id]?.trim() || id
    const removed = apps.filter((a) => row?.by[a.app_id] && !on[a.app_id])
    if (removed.length && !confirm(`Remove ${id} from ${removed.map((a) => buildLabel(a)).join(" and ")}?`)) return
    // A build whose store id changed: its old row goes (the catalog id moves to the new one).
    const renamed = apps.filter((a) => on[a.app_id] && row?.by[a.app_id] && row.by[a.app_id]!.product_id !== storeIdOf(a))
    const items = apps.filter((a) => on[a.app_id]).map((a) => ({
      app_id: a.app_id, product_id: storeIdOf(a), catalog_id: id, kind,
      default_price_micros: Math.round(Number(price || 0) * 1e6), currency: currency.trim().toUpperCase(),
      // Sent even when empty: clearing a field here clears it on every build.
      title: listing.title.trim(), description: listing.description.trim(),
      ...(kind === "subscription" ? { period: listing.period, apple_group: listing.apple_group.trim() } : {}),
    }))
    const ok = await tryAct(async () => {
      for (const a of [...removed, ...renamed]) await call("admin_delete_product", a.app_id, row!.by[a.app_id]!.product_id)
      if (items.length) await call("admin_upsert_products", JSON.stringify(items))
      return true
    }, "Saved")
    if (ok) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>{row ? row.product_id : "New product"}</DialogTitle>
        <DialogDescription>
          The product id is the app's own id: paywalls list it, entitlements are keyed by it, and the Unity config uses it. Each store uses the same id
          unless a build below names its own.
        </DialogDescription>
      </DialogHeader>
      <DialogBody>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Product id" className="sm:col-span-2">
            <Input className="font-mono text-xs" value={productId} onChange={(e) => setProductId(e.target.value)} readOnly={!!row} placeholder="e.g. vip_month" />
          </Field>
          <Field label="Type" className="sm:col-span-2">
            <Select value={kind} onValueChange={(k) => setKind(k as Kind)}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{KINDS.map((k) => <SelectItem key={k} value={k}>{k}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
          <Field label="Default price"><Input type="number" step="0.01" min="0" value={price} onChange={(e) => setPrice(e.target.value)} /></Field>
          <Field label="Currency (ISO)"><Input value={currency} onChange={(e) => setCurrency(e.target.value)} /></Field>
          <div className="space-y-2 border-t pt-4 sm:col-span-2">
            <div>
              <p className="text-sm font-medium">Builds and store ids</p>
              <p className="text-xs text-muted-foreground">Leave the store id empty when the store uses the product id.</p>
            </div>
            {apps.map((a) => (
              <div key={a.app_id} className="flex items-center gap-3">
                <div className="flex w-36 shrink-0 items-center gap-2">
                  <Checkbox id={`on-${a.app_id}`} checked={!!on[a.app_id]} onCheckedChange={(v) => setOn((o) => ({ ...o, [a.app_id]: v === true }))} />
                  <Label htmlFor={`on-${a.app_id}`} className="min-w-0 truncate font-normal">{buildLabel(a)}</Label>
                </div>
                <Input className="h-8 min-w-0 font-mono text-xs" disabled={!on[a.app_id]} value={storeIds[a.app_id] || ""} placeholder={productId.trim() || "same as the product id"}
                  onChange={(e) => setStoreIds((s) => ({ ...s, [a.app_id]: e.target.value }))} aria-label={`Store id in ${buildLabel(a)}`} />
              </div>
            ))}
          </div>
          <div className="space-y-3 border-t pt-4 sm:col-span-2">
            <div>
              <p className="text-sm font-medium">Store listing</p>
              <p className="text-xs text-muted-foreground">Optional. Fills in Create in store; the store keeps its own copy, edits there do not come back here.</p>
            </div>
            <Field label="Title"><Input value={listing.title} maxLength={55} placeholder={productId.trim() ? productId.trim().replace(/[_.-]+/g, " ") : "e.g. 100 gems"} onChange={(e) => setL({ title: e.target.value })} /></Field>
            <Field label="Description"><Textarea className="min-h-16" value={listing.description} maxLength={200} placeholder="Same as the title" onChange={(e) => setL({ description: e.target.value })} /></Field>
            {kind === "subscription" && (
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label="Period">
                  <Select value={listing.period || "none"} onValueChange={(v) => setL({ period: v === "none" ? "" : v })}>
                    <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                    <SelectContent>
                      <SelectItem value="none">Not set</SelectItem>
                      {PERIODS.map(([v, l]) => <SelectItem key={v} value={v}>{l}</SelectItem>)}
                    </SelectContent>
                  </Select>
                </Field>
                <Field label="App Store subscription group"><Input value={listing.apple_group} maxLength={64} placeholder="e.g. Premium" onChange={(e) => setL({ apple_group: e.target.value })} /></Field>
              </div>
            )}
          </div>
        </div>
      </DialogBody>
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
          <DialogDescription>
            An array of <code>{"{app_id, product_id, kind, default_price_micros, currency}"}</code> objects, optionally with <code>title</code>, <code>description</code>,{" "}
            <code>period</code> (<code>P1M</code>) and <code>apple_group</code>; existing products are updated, fields left out keep their values.
          </DialogDescription>
        </DialogHeader>
        <Textarea className="min-h-56 font-mono text-xs" value={value} onChange={(e) => setText(e.target.value)} />
        <DialogFooter><Button onClick={save}>Import</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}


/** Pasted Unity config compared with the app's products: one catalog row per product, saved to every build
 * (with the build's store-specific product id). Builds the paste names but the app lacks are only listed. */
function UnityImportDialog({ open, onClose, ...props }: UnityImportProps & { open: boolean }) {
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-3xl">{open && <UnityImport onClose={onClose} {...props} />}</DialogContent>
    </Dialog>
  )
}

interface UnityImportProps {
  apps: GameApp[]
  existing: Row[] | null
  onClose: () => void
  onSaved: () => void
  onWizard: () => void
}

function UnityImport({ apps, existing, onClose, onSaved, onWizard }: UnityImportProps) {
  const [text, setText] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [parsed, setParsed] = useState<{ rows: CatalogRow[]; storeIds: Record<string, Record<string, string>>; unknown: UnityBuild[] } | null>(null)
  const [picked, setPicked] = useState<PickedProduct[]>([])
  const onPick = useCallback((p: PickedProduct[]) => setPicked(p), [])
  /** The app's product of a build: by catalog id, else by the build's store id. */
  const find = (app: GameApp, catalogId: string, storeId: string) =>
    existing?.find((r) => r.product_id === catalogId)?.by[app.app_id] || existing?.map((r) => r.by[app.app_id]).find((p) => p?.product_id === storeId)

  const compare = () => {
    try {
      const c = parseUnityConfig(text)
      if (!c.products.length) throw new Error("The config has no products")
      const rows: CatalogRow[] = c.products.map((u) => {
        const have = apps.map((a) => find(a, u.product_id, productIdIn(u, storeOf(a).id)))
        const some = have.find(Boolean)
        const status = !some ? "new" : have.every((p) => p && p.kind === u.kind) ? "same" : "changed"
        const serverPrice = some && Number(some.default_price_micros) > 0
        return {
          product_id: u.product_id, kind: u.kind,
          price_micros: serverPrice ? Number(some.default_price_micros) : u.price_micros, currency: serverPrice ? some.currency : u.currency,
          billing_period_days: 0, period: u.period, status, local_kind: some?.kind || "",
        }
      })
      const unknown = c.builds.filter((b) => !apps.some((a) => a.app_id === b.app_id && storeOf(a).id === b.store))
      setParsed({ rows, storeIds: Object.fromEntries(c.products.map((u) => [u.product_id, u.store_ids])), unknown })
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }
  const save = async () => {
    if (!parsed) return
    const items = apps.flatMap((a) => picked.map((p) => {
      const product_id = productIdIn({ product_id: p.product_id, store_ids: parsed.storeIds[p.product_id] }, storeOf(a).id)
      const had = find(a, p.product_id, product_id)
      return {
        app_id: a.app_id, product_id, catalog_id: p.product_id, kind: p.kind,
        // The server's price wins; the Unity fallback price is used where there is none.
        ...(had && Number(had.default_price_micros) > 0
          ? { default_price_micros: Number(had.default_price_micros), currency: had.currency }
          : { default_price_micros: p.default_price_micros, currency: p.currency }),
        // Same for the period: the Unity fallback period only where the server has none.
        ...(p.period && !had?.period ? { period: p.period } : {}),
      }
    }))
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Saved ${picked.length} product(s)`)
    if (ok !== undefined) { onSaved(); onClose() }
  }
  const overrides = (r: CatalogRow) => {
    const ids = parsed?.storeIds[r.product_id] || {}
    const shown = apps.map((a) => storeOf(a).id).filter((s, i, all) => ids[s] && all.indexOf(s) === i)
    return shown.length ? shown.map((s) => `${storeById(s)?.name || s}: ${ids[s]}`).join(" · ") : null
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>Import from Unity</DialogTitle>
        <DialogDescription>
          Paste the JSON the Purser config inspector copies. Products are saved to every build of the app, with the store's own product id where the
          config names one. Prices of existing products are kept.
        </DialogDescription>
      </DialogHeader>
      <DialogBody>
        {!parsed ? (
          <div className="space-y-3">
            <Textarea className="min-h-56 font-mono text-xs" value={text} onChange={(e) => { setText(e.target.value); setError(null) }} spellCheck={false}
              placeholder={'{"serverBaseUrl": "...", "builds": [...], "products": [{"id": "gems_100", "type": "Consumable"}]}'} />
            {error && <ErrorBox error={error} />}
          </div>
        ) : (
          <div className="space-y-3">
            <CatalogPicker key={text} rows={parsed.rows} onChange={onPick} empty="The config has no products." detail={overrides} />
            {parsed.unknown.length > 0 && (
              <div className="rounded-lg border p-3 text-sm">
                <p>This app has no build for:</p>
                <ul className="mt-1 list-disc pl-5 font-mono text-xs">
                  {parsed.unknown.map((b) => <li key={`${b.store}/${b.app_id}`}>{storeById(b.store)?.name || b.store}: {b.app_id}</li>)}
                </ul>
                <p className="mt-2 text-muted-foreground">
                  Nothing is created for them here. Add them with <i>Add a build or run the setup wizard</i> on the{" "}
                  <Button variant="link" size="sm" className="h-auto p-0" onClick={() => { onClose(); onWizard() }}>Setup tab</Button>,
                  then import again.
                </p>
              </div>
            )}
          </div>
        )}
      </DialogBody>
      <DialogFooter className="gap-2">
        {parsed ? (
          <>
            <Button variant="outline" onClick={() => setParsed(null)}>Back</Button>
            <Button onClick={save} disabled={!picked.length}>Save selected</Button>
          </>
        ) : (
          <Button onClick={compare} disabled={!text.trim() || !existing}>Compare</Button>
        )}
      </DialogFooter>
    </>
  )
}
