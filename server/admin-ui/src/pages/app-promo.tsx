import { useState } from "react"
import { DicesIcon, PlusIcon, Trash2Icon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { call, select } from "@/lib/api"
import { fmtMs } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { useCan } from "@/lib/roles"
import { catalogIdOf, type Product } from "@/lib/types"
import type { TabProps } from "@/pages/app"

interface PromoRow {
  key: string
  game_key: string
  code: string
  product_id: string
  duration_days: number
  max_redemptions: number
  redeemed: number
  expires_at_ms: number
  enabled: boolean
  note: string
}

/** A code nobody guesses: 12 characters from an alphabet without look-alikes, from the browser's
 * cryptographic random source (the module's random numbers are not secret). */
function randomCode(prefix: string): string {
  const alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
  const bytes = new Uint8Array(12)
  crypto.getRandomValues(bytes)
  const body = Array.from(bytes, (b) => alphabet[b % alphabet.length]).join("")
  const p = prefix.trim().toUpperCase().replace(/[^A-Z0-9-]/g, "")
  return (p ? `${p}-` : "") + body
}

/** The app's promo codes: each gives a product of the catalog to a player once (`POST /redeem`). */
export function AppPromoTab({ data }: TabProps) {
  const g = data.group
  const ids = g.apps.map((a) => a.app_id)
  const canEdit = useCan()("configure")
  const loaded = useAsync(async () => {
    const [codes, products] = await Promise.all([
      select<PromoRow>({ table: "promo_code", where: { column: "game_key", equals: g.key } }),
      select<Product>({ table: "product", where: { column: "app_id", equals: ids } }),
    ])
    return { codes: codes.sort((a, b) => a.code.localeCompare(b.code)), products }
  }, [g.key, ids.join(",")])
  const [editing, setEditing] = useState<PromoRow | "new" | null>(null)

  if (loaded.error) return <ErrorBox error={loaded.error} />
  if (loaded.loading && !loaded.data) return <Loading rows={4} />
  const d = loaded.data!
  const kinds = new Map(d.products.map((p) => [catalogIdOf(p), p.kind]))
  const save = (row: PromoRow) => call("admin_upsert_promo_code", JSON.stringify(row))
  const remove = (row: PromoRow) =>
    tryAct(() => call("admin_delete_promo_code", g.key, row.code), `Deleted ${row.code}`).then((ok) => ok !== undefined && loaded.reload())
  const toggle = (row: PromoRow) =>
    tryAct(() => save({ ...row, enabled: !row.enabled }), row.enabled ? "Turned off" : "Turned on").then((ok) => ok !== undefined && loaded.reload())

  return (
    <Section
      title="Promo codes"
      description="A code gives a product of the catalog to each player once, without a payment (no revenue): the game gets a grant like for a purchase. Players redeem codes with RedeemCodeAsync in the Unity package (POST /redeem)."
      actions={canEdit && <Button size="sm" onClick={() => setEditing("new")}><PlusIcon /> Add code</Button>}
    >
      <RichTable
        rows={d.codes}
        rowKey={(r) => r.key}
        noun="codes"
        empty="No promo codes yet."
        columns={[
          { title: "Code", sort: (r) => r.code, cell: (r) => <Mono>{r.code}</Mono> },
          { title: "Product", sort: (r) => r.product_id, cell: (r) => <>{r.product_id}{r.duration_days > 0 && <span className="text-xs text-muted-foreground"> · {r.duration_days} days</span>}</> },
          {
            title: "Redeemed",
            num: true,
            sort: (r) => r.redeemed,
            cell: (r) => <>{r.redeemed.toLocaleString()}{r.max_redemptions > 0 && <span className="text-muted-foreground"> / {r.max_redemptions.toLocaleString()}</span>}</>,
          },
          { title: "Expires", sort: (r) => r.expires_at_ms || Infinity, cell: (r) => (r.expires_at_ms > 0 ? fmtMs(r.expires_at_ms) : "never") },
          { title: "State", sort: (r) => Number(r.enabled), cell: (r) => <Tone tone={r.enabled ? "ok" : "muted"}>{r.enabled ? "on" : "off"}</Tone> },
          { title: "Note", sort: (r) => r.note, cell: (r) => <span className="text-muted-foreground">{r.note}</span> },
          {
            title: "",
            cell: (r) =>
              canEdit && (
                <span className="flex justify-end gap-1">
                  <Button size="sm" variant="ghost" onClick={() => toggle(r)}>{r.enabled ? "Turn off" : "Turn on"}</Button>
                  <Button size="sm" variant="ghost" onClick={() => setEditing(r)}>Edit</Button>
                  <Button size="icon" variant="ghost" aria-label={`Delete ${r.code}`} onClick={() => remove(r)}><Trash2Icon /></Button>
                </span>
              ),
          },
        ]}
      />
      {editing && (
        <PromoDialog
          gameKey={g.key}
          row={editing === "new" ? null : editing}
          catalog={[...kinds.keys()].sort()}
          kinds={kinds}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null)
            loaded.reload()
          }}
        />
      )}
    </Section>
  )
}

function PromoDialog({ gameKey, row, catalog, kinds, onClose, onSaved }: {
  gameKey: string
  row: PromoRow | null
  catalog: string[]
  kinds: Map<string, string>
  onClose: () => void
  onSaved: () => void
}) {
  const [prefix, setPrefix] = useState("")
  const [code, setCode] = useState(row?.code ?? randomCode(""))
  const [product, setProduct] = useState(row?.product_id ?? catalog[0] ?? "")
  const [days, setDays] = useState(String(row?.duration_days || 30))
  const [max, setMax] = useState(String(row?.max_redemptions ?? 0))
  const [expires, setExpires] = useState(row?.expires_at_ms ? new Date(row.expires_at_ms).toISOString().slice(0, 10) : "")
  const [enabled, setEnabled] = useState(row?.enabled ?? true)
  const [note, setNote] = useState(row?.note ?? "")
  const [busy, setBusy] = useState(false)
  const subscription = kinds.get(product) === "subscription"
  const save = async () => {
    setBusy(true)
    const body = {
      game_key: gameKey,
      code,
      product_id: product,
      duration_days: subscription ? Number(days) : 0,
      max_redemptions: Number(max) || 0,
      // The end of the chosen day (UTC).
      expires_at_ms: expires ? Date.parse(`${expires}T23:59:59Z`) : 0,
      enabled,
      note,
    }
    const ok = await tryAct(() => call("admin_upsert_promo_code", JSON.stringify(body)), row ? "Saved" : `Added ${code.toUpperCase()}`)
    setBusy(false)
    if (ok !== undefined) onSaved()
  }
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{row ? `Code ${row.code}` : "New promo code"}</DialogTitle>
          <DialogDescription>Each player redeems a code once. Codes are not case-sensitive.</DialogDescription>
        </DialogHeader>
        {!row && (
          <div className="flex items-end gap-2">
            <Field label="Prefix (optional)" className="w-32">
              <Input value={prefix} onChange={(e) => setPrefix(e.target.value)} placeholder="SUMMER" />
            </Field>
            <Field label="Code" className="flex-1">
              <Input value={code} onChange={(e) => setCode(e.target.value)} className="font-mono" />
            </Field>
            <Button variant="outline" size="icon" aria-label="Generate a code" onClick={() => setCode(randomCode(prefix))}><DicesIcon /></Button>
          </div>
        )}
        <Field label="Product">
          <Select value={product} onValueChange={setProduct}>
            <SelectTrigger className="w-full"><SelectValue placeholder="A catalog product" /></SelectTrigger>
            <SelectContent>{catalog.map((c) => <SelectItem key={c} value={c}>{c} ({kinds.get(c)})</SelectItem>)}</SelectContent>
          </Select>
        </Field>
        {subscription && (
          <Field label="Access (days)">
            <Input type="number" min={1} max={3650} value={days} onChange={(e) => setDays(e.target.value)} />
          </Field>
        )}
        <div className="grid grid-cols-2 gap-3">
          <Field label="Players at most (0: no limit)">
            <Input type="number" min={0} value={max} onChange={(e) => setMax(e.target.value)} />
          </Field>
          <Field label="Valid until (empty: no end)">
            <Input type="date" value={expires} onChange={(e) => setExpires(e.target.value)} />
          </Field>
        </div>
        <Field label="Note">
          <Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="Streamer giveaway, March" />
        </Field>
        <label className="flex items-center gap-2 text-sm"><Switch checked={enabled} onCheckedChange={setEnabled} /> On</label>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={save} disabled={busy || !product || code.trim().length < 4}>{row ? "Save" : "Add"}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
