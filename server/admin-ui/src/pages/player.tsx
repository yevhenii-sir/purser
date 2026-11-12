import { useState, type ReactNode } from "react"
import { BanIcon, CheckIcon, CopyIcon, GiftIcon, RefreshCwIcon, RotateCwIcon } from "lucide-react"
import { toast } from "sonner"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Textarea } from "@/components/ui/textarea"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Stat, StatusBadge, Tone } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc } from "@/lib/api"
import { useAppName, useApps } from "@/lib/apps"
import { ago, fmtMs, fmtTime, money, usd } from "@/lib/format"
import { act, tryAct, useAsync, useCopy } from "@/lib/hooks"
import type { Purchase, PurchaseStatus } from "@/lib/types"
import { appRoute, type PageProps } from "@/routes"
import { useCan } from "@/lib/roles"
import { GrantDialog } from "@/pages/purchases"

interface ProfilePurchase extends Purchase {
  previous_user_id: string
  /** `failing`: a step failed and is retried; `pending`: the worker still has work. */
  attention: "" | "pending" | "failing"
  admin_revoke: { by: string; note: string; at_micros: number } | null
}

/** What `/entitlements` returns for one game. */
interface Entitlements {
  game_key: string
  subscriptions: Record<string, number>
  non_consumables: string[]
  server_time_ms: number
}

interface Profile {
  user_id: string
  server_time_ms: number
  builds: { app_id: string; platform?: string; game_key?: string; account?: string; deleted?: boolean }[]
  summary: {
    paid_purchases: number
    refunds: number
    sandbox_purchases: number
    manual_grants: number
    gross_usd_micros: number
    refunded_usd_micros: number
    net_usd_micros: number
    by_currency: { currency: string; gross_micros: number; refunded_micros: number }[]
    fx_missing: string[]
    first_purchase_micros: number | null
    last_purchase_micros: number | null
  }
  entitlements: Entitlements[]
  attribution: { app_id: string; ids: Record<string, string>; updated_at_micros: number }[]
  /** Per build: when the server heard from the player, the device and the game's custom attributes (missing on older modules). */
  players?: PlayerRow[]
  /** Billing country of the newest purchase that has one. */
  store_country?: string
  purchases: ProfilePurchase[]
}

interface PlayerRow {
  app_id: string
  first_seen_micros: number
  last_seen_micros: number
  client: { device: string; os: string; app_version: string; sdk_version: string }
  attributes: Record<string, string>
}

/** One build's profile: first / last seen, the device, custom attributes. */
function PlayerCard({ row, storeCountry }: { row: PlayerRow; storeCountry: string }) {
  const facts: [string, ReactNode][] = [
    ["Created", fmtTime(row.first_seen_micros)],
    ["Last seen", `${fmtTime(row.last_seen_micros)} (${ago(row.last_seen_micros)})`],
    ["Device", row.client.device],
    ["OS", row.client.os],
    ["App version", row.client.app_version],
    ["SDK version", row.client.sdk_version],
    ["Store country", storeCountry],
    ...Object.entries(row.attributes).map(([k, v]) => [k, <IdCell key={k} value={v} max={40} />] as [string, ReactNode]),
  ]
  return (
    <div className="min-w-0 space-y-2 rounded-md border p-3">
      <div className="truncate text-sm font-medium">{row.app_id}</div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        {facts.filter(([, v]) => v !== "" && v !== undefined && v !== null).map(([k, v]) => (
          <div key={k} className="contents">
            <dt className="text-muted-foreground">{k}</dt>
            <dd className="min-w-0 break-words">{v}</dd>
          </div>
        ))}
      </dl>
    </div>
  )
}

interface EntitlementRow {
  game_key: string
  product_id: string
  kind: "subscription" | "non_consumable"
  expires_ms: number
  /** Ledger rows that make it active: revoking the entitlement revokes all of them. */
  purchases: ProfilePurchase[]
}

const OWNED: readonly PurchaseStatus[] = ["verified", "granted", "completed", "transfer_pending"]
const isOwned = (p: Purchase) => OWNED.includes(p.status)

function CopyButton({ value, label = "Copy" }: { value: string; label?: string }) {
  const { copied, copy } = useCopy()
  if (!value) return null
  return (
    <Button
      variant="ghost"
      size="icon"
      className="size-6 shrink-0"
      aria-label={label}
      title={label}
      onClick={(e) => {
        e.stopPropagation()
        void copy(value)
      }}
    >
      {copied ? <CheckIcon className="size-3.5" /> : <CopyIcon className="size-3.5" />}
    </Button>
  )
}

function IdCell({ value, max = 22 }: { value: string; max?: number }) {
  if (!value) return <span className="text-muted-foreground">—</span>
  return (
    <span className="flex min-w-0 items-center gap-1">
      <Mono className="truncate" >{value.length > max ? value.slice(0, max) + "…" : value}</Mono>
      <CopyButton value={value} label="Copy id" />
    </span>
  )
}

function entitlementRows(profile: Profile): EntitlementRow[] {
  const gameOf = new Map(profile.builds.map((b) => [b.app_id, b.game_key]))
  const now = profile.server_time_ms
  const backing = (game: string, product: string, kind: EntitlementRow["kind"]) =>
    profile.purchases.filter((p) =>
      isOwned(p) && p.product_id === product && p.kind === kind && gameOf.get(p.app_id) === game &&
      (kind === "non_consumable" || !Number(p.expires_at_ms) || Number(p.expires_at_ms) > now))
  return profile.entitlements.flatMap((e) => [
    ...Object.entries(e.subscriptions).map(([product_id, expires_ms]) => ({
      game_key: e.game_key, product_id, kind: "subscription" as const, expires_ms, purchases: backing(e.game_key, product_id, "subscription"),
    })),
    ...e.non_consumables.map((product_id) => ({
      game_key: e.game_key, product_id, kind: "non_consumable" as const, expires_ms: 0, purchases: backing(e.game_key, product_id, "non_consumable"),
    })),
  ])
}

/** Confirms and revokes purchases through `admin_revoke_purchase`. */
function RevokeDialog({ target, onClose, onDone }: {
  target: { title: string; purchases: ProfilePurchase[] } | null
  onClose: () => void
  onDone: () => void
}) {
  const [reason, setReason] = useState("")
  const [busy, setBusy] = useState(false)
  const run = async () => {
    if (!target) return
    setBusy(true)
    try {
      let queued = 0
      for (const p of target.purchases) {
        const r = await act(() => proc<{ ok: boolean; status: string }>("admin_revoke_purchase", p.id, reason.trim()))
        if (r.status !== "revoked") queued++
      }
      if (queued) toast.warning("Revoked in the ledger; delivery to the game failed and is retried by the worker")
      else toast.success(target.purchases.length > 1 ? `${target.purchases.length} purchases revoked` : "Revoked")
      setReason("")
      onDone()
      onClose()
    } catch {
      onDone() // some may have gone through
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open={!!target} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Revoke {target?.title}?</DialogTitle>
          <DialogDescription>
            The game receives <Mono>revoke</Mono> with reason <Mono>admin</Mono> and <Mono>/entitlements</Mono> drops the product.
            This is not a refund: revenue and analytics stay as they are. A store purchase that is still active in the store comes
            back if the player restores it.
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-4">
          <ul className="space-y-1 text-sm">
            {target?.purchases.map((p) => (
              <li key={p.id} className="flex min-w-0 items-center gap-2">
                <StatusBadge status={p.status} />
                <Mono className="truncate">{p.id}</Mono>
              </li>
            ))}
          </ul>
          <Field label="Reason (saved in the purchase history)">
            <Textarea value={reason} onChange={(e) => setReason(e.target.value)} placeholder="e.g. chargeback reported by email" />
          </Field>
        </DialogBody>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button variant="destructive" disabled={busy || !target?.purchases.length} onClick={run}><BanIcon /> Revoke</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function requeue(p: Purchase, reload: () => void) {
  return tryAct(() => call("admin_requeue_purchase", p.id), "Queued: the worker picks it up within a minute").then(reload)
}

/** Everything support needs about one player, across every app of the database (`#player/<user_id>`). */
export function PlayerPage({ userId, go }: PageProps & { userId: string }) {
  const can = useCan()
  const [open, setOpen] = useState<string | null>(null)
  const [grant, setGrant] = useState(false)
  const [revoke, setRevoke] = useState<{ title: string; purchases: ProfilePurchase[] } | null>(null)
  const { data, error, loading, reload } = useAsync(() => proc<Profile>("admin_user_profile", userId), [userId])
  const apps = useApps().apps
  const appName = useAppName()

  if (error) return <ErrorBox error={error} hint="Player profiles need an admin token." />
  if (loading && !data) return <Loading rows={8} />
  if (!data) return null

  const s = data.summary
  const ents = entitlementRows(data)
  const problems = data.purchases.filter((p) => p.attention)
  const support = can("support")
  const seen = new Set(data.builds.map((b) => b.app_id))
  // The player's builds first in the grant dialog.
  const grantApps = [...(apps || [])].sort((a, b) => Number(seen.has(b.app_id)) - Number(seen.has(a.app_id)) || a.app_id.localeCompare(b.app_id))
  const actions = (p: ProfilePurchase): ReactNode => support && (
    <div className="flex justify-end gap-1">
      {p.attention && (
        <Button size="sm" variant="ghost" onClick={() => requeue(p, reload)} title="Retry now"><RotateCwIcon /></Button>
      )}
      {isOwned(p) && (
        <Button size="sm" variant="ghost" className="text-destructive" title="Revoke"
          onClick={() => setRevoke({ title: `${p.product_id} (${p.transaction_id})`, purchases: [p] })}>
          <BanIcon />
        </Button>
      )}
    </div>
  )

  return (
    // A grid track of minmax(0, 1fr): wide tables scroll inside their card instead of widening the page.
    <div className="grid grid-cols-[minmax(0,1fr)] gap-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0 space-y-2">
          <div className="text-xs text-muted-foreground">Player</div>
          <div className="flex min-w-0 items-center gap-1">
            <h1 className="min-w-0 font-mono text-lg font-semibold break-all">{data.user_id}</h1>
            <CopyButton value={data.user_id} label="Copy user_id" />
          </div>
          <div className="flex flex-wrap gap-1.5">
            {data.builds.length ? data.builds.map((b) => (
              <Badge key={b.app_id} variant="secondary" className={b.deleted ? "" : "cursor-pointer"}
                onClick={() => b.game_key && go(appRoute(b.game_key, "purchases"))} title={b.deleted ? "Build no longer configured" : `App ${b.game_key}`}>
                {b.app_id}{b.platform ? ` · ${b.platform}` : ""}{b.deleted ? " · deleted" : ""}
              </Badge>
            )) : <span className="text-sm text-muted-foreground">No purchases or attribution ids for this user_id yet.</span>}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="outline" onClick={reload}><RefreshCwIcon /> Refresh</Button>
          {support && <Button onClick={() => setGrant(true)}><GiftIcon /> Manual grant</Button>}
        </div>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <Stat
          label="Spent (net, USD)"
          value={usd(s.net_usd_micros)}
          hint={`${usd(s.gross_usd_micros)} gross · ${usd(s.refunded_usd_micros)} refunded${s.fx_missing.length ? ` · no rate: ${s.fx_missing.join(", ")}` : ""}`}
        />
        <Stat label="Paid purchases" value={s.paid_purchases} hint={`${s.refunds} refunded · ${s.sandbox_purchases} sandbox · ${s.manual_grants} manual`} />
        <Stat label="First purchase" value={<span className="text-base">{fmtTime(s.first_purchase_micros ?? 0)}</span>} />
        <Stat label="Last purchase" value={<span className="text-base">{fmtTime(s.last_purchase_micros ?? 0)}</span>} />
      </div>
      {s.by_currency.length > 1 && (
        <div className="flex flex-wrap gap-1.5 text-xs text-muted-foreground">
          {s.by_currency.map((c) => (
            <Badge key={c.currency} variant="outline">
              {money(c.gross_micros, c.currency)}{c.refunded_micros ? ` (−${money(c.refunded_micros, c.currency)})` : ""}
            </Badge>
          ))}
        </div>
      )}

      <Section title="Profile" description="Per build: when the server first and last heard from this player, the device the client reported and the custom attributes the game set (SetCustomAttributesAsync).">
        {data.players?.length ? (
          <div className="grid gap-4 md:grid-cols-2">
            {data.players.map((p) => <PlayerCard key={p.app_id} row={p} storeCountry={data.store_country || ""} />)}
          </div>
        ) : (
          <div className="py-4 text-center text-sm text-muted-foreground">
            The client has not reported this player yet{data.store_country ? ` · store country ${data.store_country}` : ""}.
          </div>
        )}
      </Section>

      <Section title="Entitlements" description="Active subscriptions and non-consumables, exactly as /entitlements returns them for each game.">
        <DataTable
          rows={ents}
          empty="Nothing active."
          rowKey={(r) => `${r.game_key}/${r.product_id}`}
          columns={[
            { title: "App", cell: (r) => appName(r.game_key) },
            { title: "Product", cell: (r) => <Mono>{r.product_id}</Mono> },
            { title: "Type", cell: (r) => r.kind === "subscription" ? "subscription" : "non-consumable" },
            { title: "Expires", cell: (r) => <span className="whitespace-nowrap">{r.expires_ms ? fmtMs(r.expires_ms) : "never"}</span> },
            { title: "Purchases", cell: (r) => r.purchases.length, num: true },
            {
              title: "",
              cell: (r) => (
                support && <div className="flex justify-end">
                  <Button size="sm" variant="ghost" className="text-destructive" disabled={!r.purchases.length}
                    onClick={() => setRevoke({ title: r.product_id, purchases: r.purchases })}>
                    <BanIcon /> Revoke
                  </Button>
                </div>
              ),
            },
          ]}
        />
      </Section>

      {problems.length > 0 && (
        <Section title="Delivery problems" description="Purchases the worker has not finished: failing steps are retried with backoff.">
          <DataTable
            rows={problems}
            onRowClick={(r) => setOpen(r.id)}
            rowKey={(r) => r.id}
            columns={[
              { title: "Purchase", cell: (r) => <IdCell value={r.id} max={28} /> },
              { title: "Status", cell: (r) => <span className="flex gap-1"><StatusBadge status={r.status} />{r.attention === "failing" && <Tone tone="bad">failing</Tone>}</span> },
              { title: "Attempts", cell: (r) => r.attempts, num: true },
              { title: "Next try", cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.next_attempt_at_micros)}</span> },
              { title: "Error", className: "whitespace-normal", cell: (r) => <Mono className="line-clamp-2 break-all">{r.last_error}</Mono> },
              {
                title: "",
                cell: (r) => (
                  support && <div className="flex justify-end">
                    <Button size="sm" variant="outline" onClick={() => requeue(r, reload)}><RotateCwIcon /> Retry now</Button>
                  </div>
                ),
              },
            ]}
          />
        </Section>
      )}

      <Section title="Purchases" description="Every ledger row of this player, newest first. Click a row for its history.">
        <RichTable
          rows={data.purchases}
          onRowClick={(r) => setOpen(r.id)}
          rowKey={(r) => r.id}
          empty="No purchases."
          noun="purchases"
          initialSort={{ id: "Created", desc: true }}
          columns={[
            { title: "Created", sort: (r) => r.created_at_micros, cell: (r) => <span className="whitespace-nowrap">{fmtTime(r.created_at_micros)}</span> },
            { title: "App", sort: (r) => r.app_id, cell: (r) => r.app_id },
            { title: "Product", sort: (r) => r.product_id, cell: (r) => <Mono>{r.product_id}</Mono> },
            {
              title: "Status",
              sort: (r) => r.status,
              cell: (r) => (
                <span className="flex gap-1">
                  <StatusBadge status={r.status} />
                  {r.admin_revoke && <Tone tone="muted"><span title={r.admin_revoke.note}>by admin</span></Tone>}
                </span>
              ),
            },
            { title: "Store", sort: (r) => r.store, cell: (r) => <span className="whitespace-nowrap">{r.store}{r.environment === "sandbox" ? " · sandbox" : ""}</span> },
            { title: "Price", sort: (r) => r.price_micros, cell: (r) => <span className="whitespace-nowrap">{money(r.price_micros, r.currency)}</span>, num: true },
            { title: "Expires", sort: (r) => r.expires_at_ms || null, cell: (r) => <span className="whitespace-nowrap">{fmtMs(r.expires_at_ms)}</span> },
            { title: "Transaction", sort: (r) => r.transaction_id, cell: (r) => <IdCell value={r.transaction_id} /> },
            { id: "actions", title: "", cell: actions },
          ]}
        />
      </Section>

      <Section title="Attribution ids" description="Device ids the client sent last, per build; used for events without a client (renewals, refunds).">
        {data.attribution.length ? (
          <div className="grid gap-4 md:grid-cols-2">
            {data.attribution.map((a) => (
              <div key={a.app_id} className="min-w-0 space-y-2 rounded-md border p-3">
                <div className="flex items-center justify-between gap-2 text-sm">
                  <span className="truncate font-medium">{a.app_id}</span>
                  <span className="shrink-0 text-xs text-muted-foreground">{fmtTime(a.updated_at_micros)}</span>
                </div>
                <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
                  {Object.entries(a.ids).map(([k, v]) => (
                    <div key={k} className="contents">
                      <dt className="text-muted-foreground">{k}</dt>
                      <dd className="min-w-0"><IdCell value={v} max={40} /></dd>
                    </div>
                  ))}
                </dl>
              </div>
            ))}
          </div>
        ) : (
          <div className="py-4 text-center text-sm text-muted-foreground">No attribution ids remembered for this player.</div>
        )}
      </Section>

      <PurchaseSheet id={open} onClose={(changed) => { setOpen(null); if (changed) reload() }} />
      <GrantDialog open={grant} apps={grantApps} userId={data.user_id} onClose={() => setGrant(false)} onDone={reload} />
      <RevokeDialog target={revoke} onClose={() => setRevoke(null)} onDone={reload} />
    </div>
  )
}
