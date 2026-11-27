import { useEffect, useId, useState, type ReactNode } from "react"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, Mono, Tone } from "@/components/common"
import { money } from "@/lib/format"
import { periodOfDays } from "@/lib/periods"
import type { CatalogRow, Kind } from "@/lib/types"

export interface PickedProduct {
  product_id: string
  kind: Kind
  price_micros: number
  currency: string
  /** Subscription period when the source knows it (store catalog, Unity config). */
  period?: string
  /** Product id per store id where it differs from `product_id` (from a Unity config). */
  store_ids?: Record<string, string>
  /** The store listing, when the source has one (store catalogs). */
  title?: string
  description?: string
  /** The app's own id the store id goes under (a store catalog import with a `mapping`). */
  catalog_id?: string
}

/** How a store catalog's ids map to the app's products. */
export interface CatalogMapping {
  /** The app's product ids now. */
  catalogIds: string[]
  /** The product id this build already gives the store id, if it lists it. */
  ownCatalogOf: (storeId: string) => string | undefined
  /** The store id this build already has for a product, if any. */
  ownStoreIdOf: (catalogId: string) => string | undefined
  /** The build's package / bundle id: a store id that starts with it is suggested without it. */
  appId: string
}

/** Store suffixes a store id may end with (`coins_700.ios`): the product id is suggested without them. */
const STORE_SUFFIX = /[._-](ios|android|apple|google|appstore|playstore|amazon|samsung|huawei|steam|web)$/i

/** The app's product id a store id most likely is: without the bundle in front and a store suffix. */
function suggestCatalogId(storeId: string, appId: string): string {
  let id = storeId
  if (appId && id.toLowerCase().startsWith(appId.toLowerCase() + ".")) id = id.slice(appId.length + 1)
  id = id.replace(STORE_SUFFIX, "")
  return id || storeId
}

/** Store catalog rows to pick from. The kind of one-time products can be changed: Google Play
 * does not tell consumable from non-consumable (the App Store does). */
export function CatalogPicker({ rows, onChange, empty = "The store has no products for this app.", detail, mapping }: {
  rows: CatalogRow[]
  onChange: (picked: PickedProduct[]) => void
  empty?: string
  /** Extra text under the product id. */
  detail?: (r: CatalogRow) => ReactNode
  /** Map each store id to a product of the app (a store catalog import): a column to pick it. */
  mapping?: CatalogMapping
}) {
  const [picked, setPicked] = useState<Record<string, boolean>>(() => Object.fromEntries(rows.map((r) => [r.product_id, r.status !== "same"])))
  const [kinds, setKinds] = useState<Record<string, Kind>>(() => Object.fromEntries(rows.map((r) => [r.product_id, r.kind])))
  /** The product each store id goes under: the one this build gives it, else a guess from the id. */
  const [targets, setTargets] = useState<Record<string, string>>(() =>
    Object.fromEntries(rows.map((r) => [r.product_id, mapping ? mapping.ownCatalogOf(r.product_id) ?? suggestCatalogId(r.product_id, mapping.appId) : r.product_id])))
  const listId = useId()
  // Two picked store ids under one product: the build would have two rows of it.
  const taken = new Map<string, number>()
  for (const r of rows) if (picked[r.product_id]) taken.set(targets[r.product_id]?.trim(), (taken.get(targets[r.product_id]?.trim()) || 0) + 1)
  const clash = (r: CatalogRow) => !!picked[r.product_id] && (taken.get(targets[r.product_id]?.trim()) || 0) > 1
  /** The build already has the target product under another store id. */
  const occupied = (r: CatalogRow) => {
    const other = mapping?.ownStoreIdOf(targets[r.product_id]?.trim())
    return !!picked[r.product_id] && !!other && other !== r.product_id ? other : undefined
  }

  useEffect(() => {
    onChange(
      rows
        .filter((r) => picked[r.product_id])
        .map((r) => {
          const period = kinds[r.product_id] === "subscription" ? r.period || periodOfDays(r.billing_period_days) : ""
          return {
            product_id: r.product_id, kind: kinds[r.product_id], price_micros: r.price_micros, currency: r.currency, ...(period ? { period } : {}),
            ...(r.title ? { title: r.title } : {}), ...(r.description ? { description: r.description } : {}),
            ...(mapping ? { catalog_id: targets[r.product_id]?.trim() || r.product_id } : {}),
          }
        }),
    )
  }, [rows, picked, kinds, targets, mapping, onChange])

  /** What saving does to a row: the build's own status, joining a product, or a new one. */
  const outcome = (r: CatalogRow): ReactNode => {
    if (!mapping) return <Tone tone={r.status === "new" ? "ok" : r.status === "changed" ? "warn" : "muted"}>{r.status}</Tone>
    const target = targets[r.product_id]?.trim()
    if (clash(r)) return <Tone tone="bad">twice under {target}</Tone>
    const other = occupied(r)
    if (other) return <Tone tone="bad">{target} is {other} here</Tone>
    const own = mapping.ownCatalogOf(r.product_id)
    if (own !== undefined && own === target) return <Tone tone={r.status === "changed" ? "warn" : "muted"}>{r.status}</Tone>
    if (mapping.catalogIds.includes(target)) return <Tone tone="ok">joins {target}</Tone>
    return <Tone tone="ok">new</Tone>
  }

  return (
    <>
    {mapping && <datalist id={listId}>{mapping.catalogIds.map((id) => <option key={id} value={id} />)}</datalist>}
    <DataTable
      rows={rows}
      empty={empty}
      columns={[
        {
          title: "",
          className: "w-8",
          cell: (r) => <Checkbox checked={!!picked[r.product_id]} onCheckedChange={(v) => setPicked((p) => ({ ...p, [r.product_id]: v === true }))} />,
        },
        {
          title: mapping ? "In the store" : "Product",
          cell: (r) => {
            const extra = detail?.(r)
            return <div className="min-w-0"><Mono>{r.product_id}</Mono>{extra && <div className="truncate font-mono text-xs text-muted-foreground">{extra}</div>}</div>
          },
        },
        ...(mapping ? [{
          title: "→ Product",
          cell: (r: CatalogRow) => (
            <Input className="h-8 w-44 font-mono text-xs" list={listId} value={targets[r.product_id] || ""} aria-invalid={clash(r) || !!occupied(r) || undefined}
              aria-label={`Product of ${r.product_id}`} onChange={(e) => setTargets((t) => ({ ...t, [r.product_id]: e.target.value }))} />
          ),
        }] : []),
        {
          title: "Type",
          cell: (r) =>
            r.kind === "subscription" ? (
              "subscription"
            ) : (
              <Select value={kinds[r.product_id]} onValueChange={(v) => setKinds((k) => ({ ...k, [r.product_id]: v as Kind }))}>
                <SelectTrigger size="sm" className="w-40"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="consumable">consumable</SelectItem>
                  <SelectItem value="non_consumable">non_consumable</SelectItem>
                </SelectContent>
              </Select>
            ),
        },
        { title: "Price", cell: (r) => money(r.price_micros, r.currency) + (r.billing_period_days ? ` / ${r.billing_period_days} d` : "") },
        { title: "", cell: outcome },
      ]}
    />
    </>
  )
}
