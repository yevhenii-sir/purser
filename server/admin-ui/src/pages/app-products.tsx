import { Fragment, useCallback, useMemo, useState } from "react"
import { ChevronDownIcon, ClipboardCopyIcon, ClipboardPasteIcon, CopyIcon, DownloadIcon, FileJsonIcon, PlusIcon, RefreshCwIcon, SearchIcon, StoreIcon, Trash2Icon, UploadIcon, XIcon } from "lucide-react"
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
import { DialogBody, ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { RichTable, type RichColumn } from "@/components/rich-table"
import { StoreCreateDialog, StoreJobs } from "@/components/store-create-dialog"
import { call, proc, publicRoute, select } from "@/lib/api"
import { buildLabel, creatorBuilds, storeById, storeOf } from "@/lib/stores"
import { errorText, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { catalogIdOf, KINDS, listingOf, type CatalogRow, type GameApp, type Kind, type Product } from "@/lib/types"
import { PERIODS, perPeriod } from "@/lib/periods"
import type { TabProps } from "@/pages/app"
import { appRoute } from "@/routes"
import { useCan } from "@/lib/roles"
import { parseUnityConfig, productIdIn, toUnityConfig, type UnityBuild } from "@/lib/unity-config"
import { downloadBlob } from "@/lib/utils"

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

/** The access levels of a product (the same on every build; the first build that has them). */
const accessOfRow = (r: Row) => Object.values(r.by).map((p) => p?.access || "").find(Boolean) || ""
const walletOfRow = (r: Row) => Object.values(r.by).map((p) => p?.wallet || "").find(Boolean) || ""

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
  const products = useAsync(async () => groupByCatalogId(await select<Product>({ table: "product", where: { column: "app_id", equals: ids } })), [ids.join(",")])
  const rows = useMemo(() => products.data || [], [products.data])
  const configure = useCan()("configure")
  /** Builds whose store catalog can be imported (Google Play, App Store). */
  const importers = g.apps.filter((a) => storeOf(a).product_import)
  const [query, setQuery] = useState("")
  const [kindFilter, setKindFilter] = useState<string>(ALL)
  /** Only the products this build lacks (a build key), or every product. */
  const [missingIn, setMissingIn] = useState<string>(ALL)
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase()
    return rows.filter((r) =>
      (kindFilter === ALL || r.kind === kindFilter)
      && (missingIn === ALL || !r.by[missingIn])
      && (!q || [r.product_id, ...Object.values(r.by).flatMap((p) => (p ? [p.product_id, p.title] : []))].some((t) => t?.toLowerCase().includes(q))))
  }, [rows, query, kindFilter, missingIn])
  /** The picked rows that still exist (a reload may have removed some). */
  const chosen = rows.filter((r) => picked.has(r.product_id))
  const done = (ok: unknown, keep = true) => {
    if (ok === undefined) return
    if (!keep) setPicked(new Set())
    products.reload()
  }

  // Bulk actions on the picked rows: one call each, the server applies all of it or nothing.
  const setKind = async (kind: Kind) => {
    const blocked = kind === "consumable" ? chosen.filter((r) => accessOfRow(r)) : []
    const items = chosen.filter((r) => r.kind !== "subscription" && r.kind !== kind && !blocked.includes(r)).flatMap((r) => g.apps.flatMap((a) => {
      const p = r.by[a.app_id]
      // The price goes along: a missing one would clear it.
      return p ? [{ app_id: a.app_id, product_id: p.product_id, kind, price_micros: Number(p.price_micros), currency: p.currency }] : []
    }))
    if (blocked.length) toast.warning(`${blocked.map((r) => r.product_id).join(", ")}: unlocks access levels, so it cannot be consumable`)
    if (!items.length) return toast.info(`Nothing to change: subscriptions keep their type`)
    done(await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `${new Set(items.map((i) => i.product_id)).size} product(s) are ${kind} now`))
  }
  const addTo = async (to: GameApp) => {
    const items = chosen.filter((r) => !r.by[to.app_id]).flatMap((r) => {
      const from = Object.values(r.by).find((p) => p && Number(p.price_micros) > 0) || Object.values(r.by).find(Boolean)
      if (!from) return []
      return [{
        app_id: to.app_id, product_id: from.product_id, catalog_id: r.product_id, kind: r.kind, price_micros: Number(from.price_micros), currency: from.currency,
        ...listingOf(from), ...(accessOfRow(r) ? { access: accessOfRow(r) } : {}), ...(walletOfRow(r) ? { wallet: walletOfRow(r) } : {}),
      }]
    })
    if (!items.length) return toast.info(`${buildLabel(to)} has all of them`)
    done(await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Added ${items.length} product(s) to ${buildLabel(to)}`))
  }
  const removeFrom = async (from: GameApp) => {
    const hit = chosen.filter((r) => r.by[from.app_id])
    if (!hit.length) return toast.info(`${buildLabel(from)} has none of them`)
    if (!confirm(`Remove ${hit.length} product(s) from ${buildLabel(from)}? Purchases already made stay; new ones of them are refused as unknown there.`)) return
    done(await tryAct(async () => {
      for (const r of hit) await call("admin_delete_product", from.app_id, r.by[from.app_id]!.product_id)
      return true
    }, `Removed ${hit.length} product(s) from ${buildLabel(from)}`))
  }
  const deleteAll = async () => {
    if (!confirm(`Delete ${chosen.length} product(s) from every build? Purchases already made stay in the ledger; new ones of them are refused as unknown.`)) return
    done(await tryAct(async () => {
      for (const r of chosen) for (const a of g.apps) if (r.by[a.app_id]) await call("admin_delete_product", a.app_id, r.by[a.app_id]!.product_id)
      return true
    }, `Deleted ${chosen.length} product(s)`), false)
  }

  const copyToOther = async (from: GameApp, to: GameApp) => {
    const items = rows.filter((r) => r.by[from.app_id] && !r.by[to.app_id]).map((r) => {
      const p = r.by[from.app_id]!
      return {
        app_id: to.app_id, product_id: p.product_id, catalog_id: r.product_id, kind: p.kind, price_micros: Number(p.price_micros), currency: p.currency,
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
        price_micros: Number(p.price_micros), currency: p.currency, ...listingOf(p),
      }] : []
    }))
    downloadBlob(`products-${g.key}.json`, JSON.stringify(items, null, 2) + "\n", "application/json")
  }

  /** The app's builds and products as the Unity config (inspector: Paste config from admin panel). */
  const copyUnityConfig = async () => {
    const products = rows.map((r) => {
      const priced = g.apps.map((a) => r.by[a.app_id]).find((p) => p && Number(p.price_micros) > 0)
      return {
        product_id: r.product_id, kind: r.kind, store_ids: storeIdsOf(r, g.apps),
        price_micros: priced ? Number(priced.price_micros) : 0, currency: priced?.currency || "", period: listingOfRow(r).period,
        access: accessOfRow(r).split(",").filter(Boolean),
      }
    })
    const text = toUnityConfig(await publicRoute(), g.apps.map((a) => ({ store: storeOf(a).id, store_app_id: a.store_app_id })), products)
    await tryAct(() => navigator.clipboard.writeText(text), "Copied: in Unity press Paste config from admin panel")
  }

  return (
    <>
      <Section
        title="Products"
        description="The app's products and their ids in each build's store. Each build has its own price (the App Store often sells higher): revenue uses it when the store does not report one (Google Play, Amazon), Create in store sets it in the store. The title, description and period are used by Create in store."
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
          <div className="space-y-3">
            {rows.length > 0 && (
              <div className="flex flex-wrap items-center gap-2">
                <div className="relative w-full sm:w-64">
                  <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
                  <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search by id or title" className="h-8 ps-8" />
                </div>
                <Select value={kindFilter} onValueChange={setKindFilter}>
                  <SelectTrigger size="sm" className="w-44"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value={ALL}>Every type</SelectItem>
                    {KINDS.map((k) => <SelectItem key={k} value={k}>{k}</SelectItem>)}
                  </SelectContent>
                </Select>
                {g.apps.length > 1 && (
                  <Select value={missingIn} onValueChange={setMissingIn}>
                    <SelectTrigger size="sm" className="w-56"><SelectValue /></SelectTrigger>
                    <SelectContent>
                      <SelectItem value={ALL}>In any build</SelectItem>
                      {g.apps.map((a) => <SelectItem key={a.app_id} value={a.app_id}>Missing in {buildLabel(a)}</SelectItem>)}
                    </SelectContent>
                  </Select>
                )}
              </div>
            )}
            {configure && chosen.length > 0 && (
              <div className="flex flex-wrap items-center gap-2 rounded-md border bg-muted/40 px-3 py-2">
                <span className="text-sm font-medium">{chosen.length} selected</span>
                <DropdownMenu>
                  <DropdownMenuTrigger asChild><Button variant="outline" size="sm">Type <ChevronDownIcon /></Button></DropdownMenuTrigger>
                  <DropdownMenuContent align="start">
                    <DropdownMenuItem onClick={() => setKind("consumable")}>consumable</DropdownMenuItem>
                    <DropdownMenuItem onClick={() => setKind("non_consumable")}>non_consumable</DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
                {g.apps.length > 1 && (
                  <>
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild><Button variant="outline" size="sm"><CopyIcon /> Add to <ChevronDownIcon /></Button></DropdownMenuTrigger>
                      <DropdownMenuContent align="start">{g.apps.map((a) => <DropdownMenuItem key={a.app_id} onClick={() => addTo(a)}>{buildLabel(a)}</DropdownMenuItem>)}</DropdownMenuContent>
                    </DropdownMenu>
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild><Button variant="outline" size="sm">Remove from <ChevronDownIcon /></Button></DropdownMenuTrigger>
                      <DropdownMenuContent align="start">{g.apps.map((a) => <DropdownMenuItem key={a.app_id} onClick={() => removeFrom(a)}>{buildLabel(a)}</DropdownMenuItem>)}</DropdownMenuContent>
                    </DropdownMenu>
                  </>
                )}
                {creatorBuilds(g.apps).length > 0 && (
                  <Button variant="outline" size="sm" onClick={() => setDialog("create")}><StoreIcon /> Create in store</Button>
                )}
                <Button variant="ghost" size="sm" className="text-destructive" onClick={deleteAll}><Trash2Icon /> Delete</Button>
                <Button variant="ghost" size="sm" className="ms-auto text-muted-foreground" onClick={() => setPicked(new Set())}><XIcon /> Clear</Button>
              </div>
            )}
          <RichTable
            rows={shown}
            rowKey={(r) => r.product_id}
            selected={configure ? picked : undefined}
            onSelectedChange={configure ? setPicked : undefined}
            onRowClick={configure ? setEditing : undefined}
            noun="products"
            pageSize={50}
            empty={rows.length ? "No product matches." : "No products yet."}
            columns={[
              { title: "Product", sort: (r) => r.product_id, cell: (r) => <Mono>{r.product_id}</Mono> },
              { title: "Type", sort: (r) => r.kind, cell: (r) => r.kind },
              { title: "Access", sort: (r) => accessOfRow(r), cell: (r) => accessOfRow(r) ? <span className="text-xs">{accessOfRow(r).split(",").join(", ")}</span> : <span className="text-muted-foreground">—</span> },
              { title: "Gives", sort: (r) => walletOfRow(r), cell: (r) => walletOfRow(r) ? <span className="text-xs">{walletOfRow(r).split(",").map((g) => g.replace(":", " ")).join(", ")}</span> : <span className="text-muted-foreground">—</span> },
              ...g.apps.map((a): RichColumn<Row> => ({
                id: a.app_id,
                title: buildLabel(a),
                num: true,
                sort: (r: Row) => (r.by[a.app_id] ? Number(r.by[a.app_id]!.price_micros) : null),
                cell: (r: Row) => {
                  const p = r.by[a.app_id]
                  if (!p) return <Tone tone="warn">missing</Tone>
                  const price = money(p.price_micros, p.currency)
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
          </div>
        )}
      </Section>
      {<StoreJobs key={jobsKey} apps={g.apps} onFinished={products.reload} />}
      <ProductDialog editing={editing} apps={g.apps} onClose={() => setEditing(null)} onSaved={products.reload} />
      <StoreCreateDialog open={dialog === "create"} apps={g.apps} preselected={chosen.map((r) => r.product_id)} listed={(app, id) => !!rows.find((r) => r.product_id === id)?.by[app]} onClose={() => setDialog(null)} onStarted={() => { products.reload(); setJobsKey((k) => k + 1) }}
        products={rows.map((r) => {
          const common = commonPrice(r, g.apps)
          const prices = Object.fromEntries(g.apps.flatMap((a) => {
            const p = r.by[a.app_id]
            return p && Number(p.price_micros) > 0 ? [[a.app_id, { price_micros: Number(p.price_micros), currency: p.currency }]] : []
          }))
          return { product_id: r.product_id, kind: r.kind, price_micros: common.price_micros, currency: common.currency || "USD", prices, ...listingOfRow(r) }
        })} />
      {importers.map((a) => (
        <StoreImportDialog key={a.app_id} open={dialog === `import:${a.app_id}`} app={a} others={g.apps.filter((o) => o !== a)} existing={rows} onClose={() => setDialog(null)} onSaved={products.reload} />
      ))}
      <UnityImportDialog open={dialog === "unity"} apps={g.apps} existing={products.data ? rows : null} onClose={() => setDialog(null)} onSaved={products.reload}
        onWizard={() => go(appRoute(g.key, "setup"))} />
      <JsonDialog open={dialog === "json"} appId={g.apps[0].app_id} onClose={() => setDialog(null)} onSaved={products.reload} />
    </>
  )
}

/** "Every" in the filters. */
const ALL = "__all__"

/** The price most builds of a product share (the first one on a tie); 0 when none is set. */
function commonPrice(r: Row, apps: GameApp[]): { price_micros: number; currency: string } {
  const counts = new Map<string, number>()
  for (const a of apps) {
    const p = r.by[a.app_id]
    if (p && Number(p.price_micros) > 0) {
      const k = `${Number(p.price_micros)}|${p.currency}`
      counts.set(k, (counts.get(k) || 0) + 1)
    }
  }
  const best = [...counts.entries()].reduce<[string, number] | null>((b, e) => (!b || e[1] > b[1] ? e : b), null)
  if (!best) return { price_micros: 0, currency: "" }
  const [price, currency] = best[0].split("|")
  return { price_micros: Number(price), currency }
}

const priceText = (micros?: number | string) => (Number(micros) > 0 ? String(Number(micros) / 1e6) : "")

function ProductDialog({ editing, apps, onClose, onSaved }: { editing: Row | "new" | null; apps: GameApp[]; onClose: () => void; onSaved: () => void }) {
  return (
    <Dialog open={!!editing} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        {editing && <ProductForm key={editing === "new" ? "new" : editing.product_id} row={editing === "new" ? null : editing} apps={apps} onClose={onClose} onSaved={onSaved} />}
      </DialogContent>
    </Dialog>
  )
}

function ProductForm({ row, apps, onClose, onSaved }: { row: Row | null; apps: GameApp[]; onClose: () => void; onSaved: () => void }) {
  const common = row ? commonPrice(row, apps) : { price_micros: 0, currency: "" }
  const [productId, setProductId] = useState(row?.product_id || "")
  const [kind, setKind] = useState<Kind>(row?.kind || "consumable")
  const [price, setPrice] = useState(priceText(common.price_micros))
  const [currency, setCurrency] = useState(common.currency || "USD")
  /** A build's own price where it differs from the shared one (empty: the shared price). */
  const [own, setOwn] = useState<Record<string, { price: string; currency: string }>>(() =>
    Object.fromEntries(apps.map((a) => {
      const p = row?.by[a.app_id]
      const differs = p && Number(p.price_micros) > 0 && (Number(p.price_micros) !== common.price_micros || p.currency !== common.currency)
      return [a.app_id, differs ? { price: priceText(p.price_micros), currency: p.currency } : { price: "", currency: "" }]
    })))
  const setOwnOf = (appId: string, patch: Partial<{ price: string; currency: string }>) => setOwn((o) => ({ ...o, [appId]: { ...o[appId], ...patch } }))
  const [listing, setListing] = useState<Listing>(() => (row ? listingOfRow(row) : { title: "", description: "", period: "", apple_group: "" }))
  const setL = (patch: Partial<Listing>) => setListing((l) => ({ ...l, ...patch }))
  const [access, setAccess] = useState(() => (row ? accessOfRow(row).split(",").join(", ") : ""))
  const [wallet, setWallet] = useState(() => (row ? walletOfRow(row).split(",").join(", ") : ""))
  const [on, setOn] = useState<Record<string, boolean>>(() => Object.fromEntries(apps.map((a) => [a.app_id, row ? !!row.by[a.app_id] : true])))
  /** Store id per build where it differs from the product id (Stripe Price ids, reverse-domain App Store ids). */
  const [storeIds, setStoreIds] = useState<Record<string, string>>(() =>
    Object.fromEntries(apps.map((a) => {
      const p = row?.by[a.app_id]
      return [a.app_id, p && p.product_id !== row?.product_id ? p.product_id : ""]
    })))

  /** Deletes the product from every build; purchases of it and their history stay. */
  const remove = async () => {
    if (!row) return
    const builds = apps.filter((a) => row.by[a.app_id])
    if (!confirm(`Delete ${row.product_id} from ${builds.map((a) => buildLabel(a)).join(" and ")}? Purchases already made stay in the ledger; new purchases of it are refused as unknown.`)) return
    const ok = await tryAct(async () => {
      for (const a of builds) await call("admin_delete_product", a.app_id, row.by[a.app_id]!.product_id)
      return true
    }, "Deleted")
    if (ok) { onSaved(); onClose() }
  }

  const save = async () => {
    const id = productId.trim()
    if (!id) return toast.error("Enter the product id")
    const storeIdOf = (a: GameApp) => storeIds[a.app_id]?.trim() || id
    const removed = apps.filter((a) => row?.by[a.app_id] && !on[a.app_id])
    if (removed.length && !confirm(`Remove ${id} from ${removed.map((a) => buildLabel(a)).join(" and ")}?`)) return
    // A build whose store id changed: its old row goes (the catalog id moves to the new one).
    const renamed = apps.filter((a) => on[a.app_id] && row?.by[a.app_id] && row.by[a.app_id]!.product_id !== storeIdOf(a))
    const priceOf = (a: GameApp) => {
      const o = own[a.app_id]
      const p = o?.price.trim() ? o.price : price
      return { price_micros: Math.round(Number(p || 0) * 1e6), currency: ((o?.price.trim() && o.currency.trim()) || currency).trim().toUpperCase() }
    }
    const items = apps.filter((a) => on[a.app_id]).map((a) => ({
      app_id: a.app_id, product_id: storeIdOf(a), catalog_id: id, kind, ...priceOf(a), access: kind === "consumable" ? "" : access, wallet,
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
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label="Product id" className="sm:col-span-2">
            <Input className="font-mono text-xs" value={productId} onChange={(e) => setProductId(e.target.value)} readOnly={!!row} placeholder="e.g. vip_month" />
          </Field>
          <Field label="Type" className="sm:col-span-2">
            <Select value={kind} onValueChange={(k) => setKind(k as Kind)}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{KINDS.map((k) => <SelectItem key={k} value={k}>{k}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
          {kind !== "consumable" && (
            <Field label="Access levels" className="sm:col-span-2" hint="What the product unlocks, comma-separated (premium, no_ads). The game checks a level instead of product ids, so a monthly, a yearly and a lifetime product can all unlock premium.">
              <Input className="font-mono text-xs" value={access} onChange={(e) => setAccess(e.target.value)} placeholder="e.g. premium" />
            </Field>
          )}
          <Field label="Virtual currency" className="sm:col-span-2" hint="Currency the player's wallet gets per unit bought, e.g. gems:100 or gems:100, coins:5000. Taken back when the purchase is refunded. Empty: none.">
            <Input className="font-mono text-xs" value={wallet} onChange={(e) => setWallet(e.target.value)} placeholder="e.g. gems:100" />
          </Field>
          <Field label="Price" hint="For every build without its own price below."><Input type="number" step="0.01" min="0" value={price} onChange={(e) => setPrice(e.target.value)} /></Field>
          <Field label="Currency (ISO)" hint="USD, EUR, …"><Input value={currency} maxLength={3} onChange={(e) => setCurrency(e.target.value)} /></Field>
          <div className="space-y-2 border-t pt-4 sm:col-span-2">
            <div>
              <p className="text-sm font-medium">Builds</p>
              <p className="text-xs text-muted-foreground">
                Empty fields use the product id and the price above. Give a build its own price when its store sells higher, e.g. the App Store.
              </p>
            </div>
            <div className="grid grid-cols-[9rem_minmax(0,1fr)_6rem_4.5rem] items-center gap-x-3 gap-y-2">
              <span />
              <span className="text-xs text-muted-foreground">Store id</span>
              <span className="text-xs text-muted-foreground">Own price</span>
              <span className="text-xs text-muted-foreground">Currency</span>
              {apps.map((a) => (
                <Fragment key={a.app_id}>
                  <div className="flex min-w-0 items-center gap-2">
                    <Checkbox id={`on-${a.app_id}`} checked={!!on[a.app_id]} onCheckedChange={(v) => setOn((o) => ({ ...o, [a.app_id]: v === true }))} />
                    <Label htmlFor={`on-${a.app_id}`} className="min-w-0 truncate font-normal">{buildLabel(a)}</Label>
                  </div>
                  <Input className="h-8 min-w-0 font-mono text-xs" disabled={!on[a.app_id]} value={storeIds[a.app_id] || ""} placeholder={productId.trim() || "same as the product id"}
                    onChange={(e) => setStoreIds((s) => ({ ...s, [a.app_id]: e.target.value }))} aria-label={`Store id in ${buildLabel(a)}`} />
                  <Input className="h-8 tabular-nums" type="number" step="0.01" min="0" disabled={!on[a.app_id]} value={own[a.app_id]?.price || ""} placeholder={price || "—"}
                    onChange={(e) => setOwnOf(a.app_id, { price: e.target.value })} aria-label={`Price in ${buildLabel(a)}`} />
                  <Input className="h-8" maxLength={3} disabled={!on[a.app_id] || !own[a.app_id]?.price.trim()} value={own[a.app_id]?.currency || ""} placeholder={currency || "USD"}
                    onChange={(e) => setOwnOf(a.app_id, { currency: e.target.value })} aria-label={`Currency in ${buildLabel(a)}`} />
                </Fragment>
              ))}
            </div>
          </div>
          <div className="space-y-3 border-t pt-4 sm:col-span-2">
            <div>
              <p className="text-sm font-medium">Store listing</p>
              <p className="text-xs text-muted-foreground">Optional. Fills in Create in store; the store keeps its own copy, edits there do not come back here.</p>
            </div>
            <Field label="Title"><Input value={listing.title} maxLength={55} placeholder={productId.trim() ? productId.trim().replace(/[_.-]+/g, " ") : "e.g. 100 gems"} onChange={(e) => setL({ title: e.target.value })} /></Field>
            <Field label="Description"><Textarea className="min-h-16" value={listing.description} maxLength={200} placeholder="Same as the title" onChange={(e) => setL({ description: e.target.value })} /></Field>
            {kind === "subscription" && (
              <div className="grid items-start gap-4 sm:grid-cols-2">
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
      <DialogFooter className="gap-2 sm:justify-between">
        <div>{row && <Button variant="ghost" className="text-destructive" onClick={remove}><Trash2Icon /> Delete</Button>}</div>
        <Button onClick={save}>Save</Button>
      </DialogFooter>
    </>
  )
}

/** What each store's import needs and leaves to the admin. */
const IMPORT_HELP: Record<string, string> = {
  google: 'Pick consumable or non-consumable for one-time products: Google Play does not say. Needs the permission "View app information" for the service account.',
  apple: "Needs an App Store Connect API Team key (App Manager role) in the build's keys: the In-App Purchase key cannot list products. Prices are the US ones (else the base territory's).",
}

function StoreImportDialog({ open, app, others, existing, onClose, onSaved }: {
  open: boolean; app: GameApp; others: GameApp[]
  /** The app's products now, grouped by catalog id. */
  existing: Row[]
  onClose: () => void; onSaved: () => void
}) {
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
    const items = picked.flatMap((p) => {
      // The product this store id already is here (its catalog id may differ), else one of that catalog id.
      const row = existing.find((r) => r.by[app.app_id]?.product_id === p.product_id) || existing.find((r) => r.product_id === p.product_id)
      const catalog_id = row?.product_id || p.product_id
      // A store that reports no price (the App Store) keeps the price this product has already.
      const known = Object.values(row?.by || {}).find((x) => x && Number(x.price_micros) > 0)
      const priceFor = (own?: Product) => (p.price_micros > 0 ? { price_micros: p.price_micros, currency: p.currency }
        : own && Number(own.price_micros) > 0 ? { price_micros: Number(own.price_micros), currency: own.currency }
        : known ? { price_micros: Number(known.price_micros), currency: known.currency } : { price_micros: 0, currency: p.currency })
      // The store listing fills only what is empty here: a title or description typed here stays.
      const { title, description, ...rest } = p
      const had = row ? listingOfRow(row) : { title: "", description: "" }
      const listing = { ...(title && !had.title ? { title } : {}), ...(description && !had.description ? { description } : {}) }
      const mine = { ...rest, app_id: app.app_id, catalog_id, ...priceFor(row?.by[app.app_id]), ...listing }
      // The other builds get only what they do not have: their own rows are never overwritten.
      const theirs = alsoOthers ? others.filter((o) => !row?.by[o.app_id]).map((o) => ({ ...rest, app_id: o.app_id, catalog_id, ...priceFor(), ...listing })) : []
      return [mine, ...theirs]
    })
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Saved ${picked.length} product(s)`)
    if (ok !== undefined) { onSaved(); onClose(); setRows(null) }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => { if (!o) onClose(); else if (!rows && !loading) void load() }}>
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Import from {storeOf(app).name}</DialogTitle>
          <DialogDescription>The catalog of {app.store_app_id}. {IMPORT_HELP[storeOf(app).id] || ""}</DialogDescription>
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
              <Label htmlFor="also" className="font-normal">Also add to {others.map((o) => buildLabel(o)).join(", ")} <span className="text-muted-foreground">(what they do not have yet)</span></Label>
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
  const value = text ?? `[\n  {"app_id": "${appId}", "product_id": "gems_100", "kind": "consumable", "price_micros": 990000, "currency": "USD"}\n]`
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
            An array of <code>{"{app_id, product_id, kind, price_micros, currency}"}</code> objects, optionally with <code>title</code>, <code>description</code>,{" "}
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
  const [parsed, setParsed] = useState<{ rows: CatalogRow[]; storeIds: Record<string, Record<string, string>>; access: Record<string, string[]>; unknown: UnityBuild[] } | null>(null)
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
        const serverPrice = some && Number(some.price_micros) > 0
        return {
          product_id: u.product_id, kind: u.kind,
          price_micros: serverPrice ? Number(some.price_micros) : u.price_micros, currency: serverPrice ? some.currency : u.currency,
          billing_period_days: 0, period: u.period, status, local_kind: some?.kind || "",
        }
      })
      const unknown = c.builds.filter((b) => !apps.some((a) => a.store_app_id === b.store_app_id && storeOf(a).id === b.store))
      setParsed({ rows, storeIds: Object.fromEntries(c.products.map((u) => [u.product_id, u.store_ids])), access: Object.fromEntries(c.products.map((u) => [u.product_id, u.access])), unknown })
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
        ...(had && Number(had.price_micros) > 0
          ? { price_micros: Number(had.price_micros), currency: had.currency }
          : { price_micros: p.price_micros, currency: p.currency }),
        // Same for the period: the Unity fallback period only where the server has none.
        ...(p.period && !had?.period ? { period: p.period } : {}),
        // Access levels of the config, where the server has none yet.
        ...(parsed.access[p.product_id]?.length && !had?.access ? { access: parsed.access[p.product_id].join(",") } : {}),
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
                  {parsed.unknown.map((b) => <li key={`${b.store}/${b.store_app_id}`}>{storeById(b.store)?.name || b.store}: {b.store_app_id}</li>)}
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
