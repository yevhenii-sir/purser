import { useRef, useState } from "react"
import { CalendarPlusIcon, RotateCwIcon, Undo2Icon } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { DataTable, ErrorBox, Field, Loading, Mono, StatusBadge } from "@/components/common"
import { call, proc, select } from "@/lib/api"
import { fmtMs, fmtTime, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { useCan } from "@/lib/roles"
import type { Purchase } from "@/lib/types"
import { playerRoute } from "@/routes"

interface LogRow {
  id: number
  event: string
  details: string
  at_micros: number
}

/** `onClose(changed)`: `changed` when the purchase was queued again while the sheet was open. */
export function PurchaseSheet({ id, onClose }: { id: string | null; onClose: (changed: boolean) => void }) {
  const changed = useRef(false)
  const close = () => {
    onClose(changed.current)
    changed.current = false
  }
  const canRetry = useCan()("support")
  const { data, loading, error, reload } = useAsync(async () => {
    if (!id) return null
    const [p] = await select<Purchase>({ table: "purchase", where: { column: "id", equals: id } })
    const log = (await select<LogRow>({ table: "purchase_log", where: { column: "purchase_id", equals: id } })).sort((a, b) => Number(a.id) - Number(b.id))
    return { p, log }
  }, [id])
  const p = data?.p
  const [action, setAction] = useState<"refund" | "extend" | null>(null)
  const owned = !!p && ["verified", "granted", "completed"].includes(p.status)
  const canRefund = canRetry && owned && p?.store === "google"
  const canExtend = canRetry && owned && p?.kind === "subscription" && (p?.store === "google" || p?.store === "apple")
  const done = () => {
    setAction(null)
    changed.current = true
    reload()
  }
  return (
    <Sheet open={!!id} onOpenChange={(open) => !open && close()}>
      <SheetContent className="w-full gap-0 sm:max-w-2xl">
        <SheetHeader>
          <SheetTitle className="font-mono text-sm break-all">{id}</SheetTitle>
          <SheetDescription>Ledger row and everything that happened to it.</SheetDescription>
        </SheetHeader>
        <ScrollArea className="min-h-0 flex-1">
          <div className="min-w-0 space-y-6 px-4 pb-6 [&_table]:w-full">
            {error ? (
              <ErrorBox error={error} />
            ) : loading || !data ? (
              <Loading />
            ) : !p ? (
              <p className="text-sm text-muted-foreground">Purchase not found.</p>
            ) : (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <StatusBadge status={p.status} />
                  <Badge variant="secondary">{p.store}</Badge>
                  <Badge variant="secondary">{p.environment}</Badge>
                  <span className="flex-1" />
                  {/* Only for rows with work left, and roles that may act on purchases. */}
                  {canRetry && Number(p.next_attempt_at_micros) > 0 && (
                    <Button size="sm" variant="outline" onClick={() => tryAct(() => call("admin_requeue_purchase", p.id), "Queued: the worker picks it up within a minute").then((ok) => {
                      if (ok !== undefined) {
                        changed.current = true
                        reload()
                      }
                    })}>
                      <RotateCwIcon /> Retry now
                    </Button>
                  )}
                  {canExtend && (
                    <Button size="sm" variant="outline" onClick={() => setAction("extend")}>
                      <CalendarPlusIcon /> Extend
                    </Button>
                  )}
                  {canRefund && (
                    <Button size="sm" variant="outline" onClick={() => setAction("refund")}>
                      <Undo2Icon /> Refund
                    </Button>
                  )}
                </div>
                {action === "refund" && <RefundDialog purchase={p} onClose={() => setAction(null)} onDone={done} />}
                {action === "extend" && <ExtendDialog purchase={p} onClose={() => setAction(null)} onDone={done} />}
                <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
                  {[
                    ["App", p.app_id],
                    ["Product", `${p.product_id} (${p.kind})`],
                    ["Player", <a key="u" href={"#" + playerRoute(p.user_id)} onClick={close} className="hover:underline" title="Open the player"><Mono className="break-all">{p.user_id}</Mono></a>],
                    ["Transaction", <Mono key="t" className="break-all">{p.transaction_id}</Mono>],
                    ["Price", <PriceText key="p" purchase={p} />],
                    ["Reason", p.reason],
                    ["Expires", fmtMs(p.expires_at_ms)],
                    ...(p.price_change ? [["Price change", priceChangeText(p)]] : []),
                    ["Created", fmtTime(p.created_at_micros)],
                    ["Attempts", String(p.attempts)],
                    ["Next attempt", fmtTime(p.next_attempt_at_micros)],
                  ].map(([k, v], i) => (
                    <div key={i} className="contents">
                      <dt className="text-muted-foreground">{k}</dt>
                      <dd className="min-w-0">{v}</dd>
                    </div>
                  ))}
                </dl>
                {p.last_error && <pre className="rounded-md bg-destructive/10 p-3 font-mono text-xs whitespace-pre-wrap text-destructive">{p.last_error}</pre>}
                <div className="space-y-2">
                  <h3 className="text-sm font-medium">History</h3>
                  <DataTable
                    rows={data.log}
                    columns={[
                      { title: "Time", cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.at_micros)}</span> },
                      { title: "Event", cell: (r) => r.event },
                      { title: "Details", className: "whitespace-normal", cell: (r) => <Mono className="break-all">{r.details}</Mono> },
                    ]}
                  />
                </div>
                <details>
                  <summary className="cursor-pointer text-sm text-muted-foreground">Raw row</summary>
                  <pre className="mt-2 rounded-md bg-muted p-3 font-mono text-xs whitespace-pre-wrap break-all">{JSON.stringify(p, null, 2)}</pre>
                </details>
              </>
            )}
          </div>
        </ScrollArea>
      </SheetContent>
    </Sheet>
  )
}

/** Refunds a Google Play order in the store (`admin_refund_purchase`): the money goes back and access ends. */
/** The price with its units, the tax the store reported and what was refunded. */
/** A price change of the next renewals the store announced. */
function priceChangeText(p: Purchase): string {
  const to = p.price_change_micros > 0 ? ` to ${money(p.price_change_micros, p.currency)}` : ""
  switch (p.price_change) {
    case "consent_needed":
      return `increase${to}: the player has not accepted it (the subscription ends unless they do)`
    case "accepted":
      return `increase${to}, accepted`
    default:
      return `next renewals${to}`
  }
}

function PriceText({ purchase: p }: { purchase: Purchase }) {
  const notes: string[] = []
  if (p.quantity > 1) notes.push(`${p.quantity} units`)
  if (p.tax_micros > 0) notes.push(`tax ${money(p.tax_micros, p.currency)}`)
  if (p.refunded_micros > 0) notes.push(`${money(p.refunded_micros, p.currency)} refunded`)
  if (p.refund_share_milli > 0) notes.push(`refund of ${p.refund_share_milli / 1000}%`)
  return (
    <>
      {money(p.price_micros, p.currency)}
      {notes.length > 0 && <span className="text-muted-foreground"> · {notes.join(" · ")}</span>}
    </>
  )
}

function RefundDialog({ purchase, onClose, onDone }: { purchase: Purchase; onClose: () => void; onDone: () => void }) {
  const [note, setNote] = useState("")
  const [busy, setBusy] = useState(false)
  const refund = async () => {
    setBusy(true)
    const ok = await tryAct(() => proc("admin_refund_purchase", purchase.id, note), "Refunded in Google Play")
    setBusy(false)
    if (ok !== undefined) onDone()
  }
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Refund in Google Play</DialogTitle>
          <DialogDescription>
            Google returns {money(purchase.price_micros, purchase.currency)} to the player and takes the access away (a subscription
            ends now). The game gets a revoke and revenue counts a refund. This cannot be undone.
          </DialogDescription>
        </DialogHeader>
        <Field label="Note (kept in the purchase's history)">
          <Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="charged twice" />
        </Field>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button variant="destructive" onClick={refund} disabled={busy}>Refund</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

const APPLE_REASONS = [
  { id: "1", label: "Customer satisfaction" },
  { id: "3", label: "Service issue or outage" },
  { id: "2", label: "Other" },
  { id: "0", label: "Undeclared" },
]

/** Moves the subscription's renewal later in the store (`admin_extend_subscription`). */
function ExtendDialog({ purchase, onClose, onDone }: { purchase: Purchase; onClose: () => void; onDone: () => void }) {
  const apple = purchase.store === "apple"
  const max = apple ? 90 : 365
  const [days, setDays] = useState("7")
  const [reason, setReason] = useState("1")
  const [busy, setBusy] = useState(false)
  const n = Number(days)
  const valid = Number.isInteger(n) && n >= 1 && n <= max
  const extend = async () => {
    setBusy(true)
    const ok = await tryAct(() => proc("admin_extend_subscription", purchase.id, n, Number(apple ? reason : 0)), `Extended by ${n} day(s)`)
    setBusy(false)
    if (ok !== undefined) onDone()
  }
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Extend the subscription</DialogTitle>
          <DialogDescription>
            The next renewal moves later and the player keeps access without paying until then (now it ends {fmtMs(purchase.expires_at_ms)}).
            {apple ? " The App Store allows two extensions a year per subscription." : ""}
          </DialogDescription>
        </DialogHeader>
        <Field label={`Days (1-${max})`}>
          <Input type="number" min={1} max={max} value={days} onChange={(e) => setDays(e.target.value)} />
        </Field>
        {apple && (
          <Field label="Reason (sent to Apple)">
            <Select value={reason} onValueChange={setReason}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{APPLE_REASONS.map((r) => <SelectItem key={r.id} value={r.id}>{r.label}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={extend} disabled={busy || !valid}>Extend</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
