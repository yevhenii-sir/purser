import { useRef } from "react"
import { RotateCwIcon } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { DataTable, Loading, Mono, StatusBadge } from "@/components/common"
import { call, sql, sqlStr } from "@/lib/api"
import { fmtMs, fmtTime, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
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
  const { data, loading } = useAsync(async () => {
    if (!id) return null
    const [p] = await sql<Purchase>(`SELECT * FROM purchase WHERE id = ${sqlStr(id)}`)
    const log = (await sql<LogRow>(`SELECT * FROM purchase_log WHERE purchase_id = ${sqlStr(id)}`)).sort((a, b) => Number(a.id) - Number(b.id))
    return { p, log }
  }, [id])
  const p = data?.p
  return (
    <Sheet open={!!id} onOpenChange={(open) => !open && close()}>
      <SheetContent className="w-full gap-0 sm:max-w-2xl">
        <SheetHeader>
          <SheetTitle className="font-mono text-sm break-all">{id}</SheetTitle>
          <SheetDescription>Ledger row and everything that happened to it.</SheetDescription>
        </SheetHeader>
        <ScrollArea className="min-h-0 flex-1">
          <div className="min-w-0 space-y-6 px-4 pb-6 [&_table]:w-full">
            {loading || !data ? (
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
                  <Button size="sm" variant="outline" onClick={() => tryAct(() => call("admin_requeue_purchase", p.id), "Queued: the worker picks it up within a minute").then((ok) => { if (ok !== undefined) changed.current = true })}>
                    <RotateCwIcon /> Retry now
                  </Button>
                </div>
                <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
                  {[
                    ["App", p.app_id],
                    ["Product", `${p.product_id} (${p.kind})`],
                    ["Player", <a key="u" href={"#" + playerRoute(p.user_id)} onClick={close} className="hover:underline" title="Open the player"><Mono className="break-all">{p.user_id}</Mono></a>],
                    ["Transaction", <Mono key="t" className="break-all">{p.transaction_id}</Mono>],
                    ["Price", money(p.price_micros, p.currency)],
                    ["Reason", p.reason],
                    ["Expires", fmtMs(p.expires_at_ms)],
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
