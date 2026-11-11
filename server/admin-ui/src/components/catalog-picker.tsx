import { useEffect, useState, type ReactNode } from "react"
import { Checkbox } from "@/components/ui/checkbox"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, Mono, Tone } from "@/components/common"
import { money } from "@/lib/format"
import { periodOfDays } from "@/lib/periods"
import type { CatalogRow, Kind } from "@/lib/types"

export interface PickedProduct {
  product_id: string
  kind: Kind
  default_price_micros: number
  currency: string
  /** Subscription period when the source knows it (store catalog, Unity config). */
  period?: string
  /** Product id per store id where it differs from `product_id` (from a Unity config). */
  store_ids?: Record<string, string>
}

/** Store catalog rows to pick from. The kind of one-time products can be changed: Google Play
 * does not tell consumable from non-consumable (the App Store does). */
export function CatalogPicker({ rows, onChange, empty = "The store has no products for this app.", detail }: {
  rows: CatalogRow[]
  onChange: (picked: PickedProduct[]) => void
  empty?: string
  /** Extra text under the product id. */
  detail?: (r: CatalogRow) => ReactNode
}) {
  const [picked, setPicked] = useState<Record<string, boolean>>(() => Object.fromEntries(rows.map((r) => [r.product_id, r.status !== "same"])))
  const [kinds, setKinds] = useState<Record<string, Kind>>(() => Object.fromEntries(rows.map((r) => [r.product_id, r.kind])))

  useEffect(() => {
    onChange(
      rows
        .filter((r) => picked[r.product_id])
        .map((r) => {
          const period = kinds[r.product_id] === "subscription" ? r.period || periodOfDays(r.billing_period_days) : ""
          return { product_id: r.product_id, kind: kinds[r.product_id], default_price_micros: r.price_micros, currency: r.currency, ...(period ? { period } : {}) }
        }),
    )
  }, [rows, picked, kinds, onChange])

  return (
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
          title: "Product",
          cell: (r) => {
            const extra = detail?.(r)
            return <div className="min-w-0"><Mono>{r.product_id}</Mono>{extra && <div className="truncate font-mono text-xs text-muted-foreground">{extra}</div>}</div>
          },
        },
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
        { title: "", cell: (r) => <Tone tone={r.status === "new" ? "ok" : r.status === "changed" ? "warn" : "muted"}>{r.status}</Tone> },
      ]}
    />
  )
}
