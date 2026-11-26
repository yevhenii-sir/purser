import { useState, type ReactNode } from "react"
import { ArrowRightLeftIcon, BanIcon, CheckIcon, CopyIcon, DownloadIcon, EraserIcon, GiftIcon, RefreshCwIcon, RotateCwIcon } from "lucide-react"
import { toast } from "sonner"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Stat, StatusBadge, Tone } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { PurchaseSheet } from "@/components/purchase-sheet"
import { call, proc } from "@/lib/api"
import { useAppName, useApps } from "@/lib/apps"
import { ago, fmtMs, fmtTime, money, usd } from "@/lib/format"
import { act, tryAct, useAsync, useCopy } from "@/lib/hooks"
import type { Purchase, PurchaseStatus } from "@/lib/types"
import { appRoute, playerRoute, type PageProps } from "@/routes"
import { downloadBlob } from "@/lib/utils"
import { useCan } from "@/lib/roles"
import { GrantDialog } from "@/pages/purchases"

interface ProfilePurchase extends Purchase {
  previous_user_id: string
  /** `failing`: a step failed and is retried; `pending`: the worker still has work. */
  attention: "" | "pending" | "failing"
  admin_revoke: { by: string; note: string; at_micros: number } | null
}

/** An access level or subscription in `/entitlements`. */
interface EntitlementState {
  product_id: string
  store: string
  expires_at_ms: number
  will_renew: boolean
  in_grace: boolean
  billing_issue: boolean
  trial: boolean
  renews_to?: string
}

/** What `/entitlements` returns for one game. */
interface Entitlements {
  game_key: string
  access: Record<string, EntitlementState>
  subscriptions: Record<string, EntitlementState>
  non_consumables: string[]
  server_time_ms: number
}

interface WalletBalance { game_key: string; currency: string; balance: number; updated_at_micros: number }
interface WalletTx { game_key: string; currency: string; delta: number; balance_after: number; reason: string; reference: string; actor: string; note: string; at_micros: number }

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
  /** Virtual currency per app (`wallet` of products, `/server/wallet`). */
  wallet?: { balances: WalletBalance[]; transactions: WalletTx[] }
  /** Per build: when the server heard from the player, the device and the game's custom attributes. */
  players: PlayerRow[]
  /** Billing country of the newest purchase that has one ("" if none). */
  store_country: string
  /** The newest purchases, and older ones that back an entitlement or need attention. */
  purchases: ProfilePurchase[]
  /** Every purchase of the player (`purchases` may list fewer). */
  purchases_total: number
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
  /** Subscriptions: renewal state; the access levels the product gives. */
  state?: EntitlementState
  access: string[]
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
  const levels = (e: Entitlements, product: string) => Object.entries(e.access || {}).filter(([, s]) => s.product_id === product).map(([l]) => l)
  return profile.entitlements.flatMap((e) => [
    ...Object.entries(e.subscriptions).map(([product_id, state]) => ({
      game_key: e.game_key, product_id, kind: "subscription" as const, expires_ms: state.expires_at_ms, state, access: levels(e, product_id),
      purchases: backing(e.game_key, product_id, "subscription"),
    })),
    ...e.non_consumables.map((product_id) => ({
      game_key: e.game_key, product_id, kind: "non_consumable" as const, expires_ms: 0, access: levels(e, product_id),
      purchases: backing(e.game_key, product_id, "non_consumable"),
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
  const [moving, setMoving] = useState<ProfilePurchase | null>(null)
  const [adjust, setAdjust] = useState<{ game_key: string; currency: string } | null>(null)
  const { data, error, loading, reload } = useAsync(() => proc<Profile>("admin_user_profile", userId), [userId])
  const apps = useApps().apps
  const appName = useAppName()

  if (error) return <ErrorBox error={error} hint="Player profiles need an admin token." />
  if (loading && !data) return <Loading rows={8} />
  if (!data) return null

  const s = data.summary
  const ents = entitlementRows(data)
  const games = [...new Set(data.builds.map((b) => b.game_key).filter((g): g is string => !!g))]
  const exportData = async () => {
    const out = await tryAct(async () => {
      const parts = []
      for (const g of games) parts.push(await proc<unknown>("admin_export_player", g, data.user_id))
      return parts
    })
    if (out) downloadBlob(`player-${data.user_id}.json`, JSON.stringify({ user_id: data.user_id, apps: out }, null, 2), "application/json")
  }
  const erase = async () => {
    const typed = prompt(`Erase ${data.user_id} from ${games.map(appName).join(", ")}? The profile, attributes and attribution ids are deleted; purchases stay for revenue under an anonymous id, and the player loses what they own. Type the user id to confirm.`)
    if (typed !== data.user_id) return
    const ok = await tryAct(async () => {
      for (const g of games) await call("admin_erase_player", g, data.user_id)
      return true
    }, "Erased")
    if (ok) reload()
  }
  const move = async (p: ProfilePurchase, to: string) => {
    const r = await tryAct(() => proc<{ ok: boolean; status: string }>("admin_transfer_purchase", p.id, to.trim()), "Moved")
    if (r) { setMoving(null); go(playerRoute(to.trim())) }
  }
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
      {isOwned(p) && p.kind !== "consumable" && (
        <Button size="sm" variant="ghost" title="Move to another player" onClick={() => setMoving(p)}><ArrowRightLeftIcon /></Button>
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
          {support && games.length > 0 && <Button variant="outline" onClick={exportData}><DownloadIcon /> Export data</Button>}
          {support && games.length > 0 && <Button variant="outline" className="text-destructive" onClick={erase}><EraserIcon /> Erase</Button>}
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
        {data.players.length ? (
          <div className="grid gap-4 md:grid-cols-2">
            {data.players.map((p) => <PlayerCard key={p.app_id} row={p} storeCountry={data.store_country} />)}
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
            { title: "Access", cell: (r) => r.access.length ? <span className="text-xs">{r.access.join(", ")}</span> : <span className="text-muted-foreground">—</span> },
            { title: "Expires", cell: (r) => <span className="whitespace-nowrap">{r.expires_ms ? fmtMs(r.expires_ms) : "never"}</span> },
            {
              title: "Renewal",
              cell: (r) => !r.state ? <span className="text-muted-foreground">—</span>
                : r.state.billing_issue ? <Tone tone="bad">{r.state.in_grace ? "payment failed · grace period" : "payment failed"}</Tone>
                : r.state.will_renew ? <Tone tone="ok">{r.state.renews_to ? `renews as ${r.state.renews_to}` : r.state.trial ? "trial, renews" : "renews"}</Tone>
                : <Tone tone="warn">auto-renew off</Tone>,
            },
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

      <Section
        title="Purchases"
        description={
          data.purchases_total > data.purchases.length
            ? `The newest of ${data.purchases_total} ledger rows of this player, and older ones that back an entitlement or are not finished. Click a row for its history.`
            : "Every ledger row of this player, newest first. Click a row for its history."
        }
      >
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

      {data.wallet && (data.wallet.balances.length > 0 || data.wallet.transactions.length > 0) && (
        <Section
          title="Wallet"
          description="Virtual currency: credited by purchases of products that give it, taken back by their refunds, spent and given by the game."
          actions={support && games.length > 0 && (
            <Button size="sm" variant="outline" onClick={() => setAdjust({ game_key: games[0], currency: data.wallet!.balances[0]?.currency || "" })}>Adjust</Button>
          )}
        >
          <div className="mb-3 flex flex-wrap gap-3">
            {data.wallet.balances.map((b) => (
              <div key={`${b.game_key}/${b.currency}`} className="min-w-32 rounded-md border px-3 py-2">
                <div className="text-xs text-muted-foreground">{b.currency}{games.length > 1 && ` · ${appName(b.game_key)}`}</div>
                <div className={b.balance < 0 ? "text-lg font-semibold text-destructive tabular-nums" : "text-lg font-semibold tabular-nums"}>{b.balance.toLocaleString()}</div>
              </div>
            ))}
          </div>
          <DataTable
            rows={data.wallet.transactions}
            empty="No changes yet."
            columns={[
              { title: "Time", cell: (t) => <span className="whitespace-nowrap">{fmtTime(t.at_micros)}</span> },
              { title: "Change", className: "text-right tabular-nums", cell: (t) => <span className={t.delta < 0 ? "text-destructive" : ""}>{t.delta > 0 ? "+" : ""}{t.delta.toLocaleString()} {t.currency}</span> },
              { title: "Balance", className: "text-right tabular-nums", cell: (t) => t.balance_after.toLocaleString() },
              { title: "Why", cell: (t) => <span>{t.reason}{t.note && <span className="text-muted-foreground"> · {t.note}</span>}</span> },
              { title: "Reference", className: "whitespace-normal", cell: (t) => <Mono className="break-all text-xs">{t.reference}</Mono> },
            ]}
          />
        </Section>
      )}

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
      <MoveDialog purchase={moving} onClose={() => setMoving(null)} onMove={move} />
      <RevokeDialog target={revoke} onClose={() => setRevoke(null)} onDone={reload} />
      <WalletDialog target={adjust} games={games} userId={data.user_id} onClose={() => setAdjust(null)} onDone={reload} />
    </div>
  )
}

/** Asks for the player a purchase moves to (`admin_transfer_purchase`). */
function MoveDialog({ purchase, onClose, onMove }: { purchase: ProfilePurchase | null; onClose: () => void; onMove: (p: ProfilePurchase, to: string) => void }) {
  const [to, setTo] = useState("")
  return (
    <Dialog open={!!purchase} onOpenChange={(o) => { if (!o) { onClose(); setTo("") } }}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Move {purchase?.product_id} to another player</DialogTitle>
          <DialogDescription>
            The game gets revoke (reason transfer) for this player, then grant for the new one. The app's transfer policy does not apply to support.
          </DialogDescription>
        </DialogHeader>
        <Field label="New user id"><Input className="font-mono text-xs" value={to} onChange={(e) => setTo(e.target.value)} placeholder="user id" /></Field>
        <DialogFooter><Button disabled={!to.trim() || !purchase} onClick={() => purchase && onMove(purchase, to)}><ArrowRightLeftIcon /> Move</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/** Changes a player's balance (Support): a positive or negative amount and a note for the history. */
function WalletDialog({ target, games, userId, onClose, onDone }: {
  target: { game_key: string; currency: string } | null
  games: string[]
  userId: string
  onClose: () => void
  onDone: () => void
}) {
  const appName = useAppName()
  const [game, setGame] = useState("")
  const [currency, setCurrency] = useState("")
  const [amount, setAmount] = useState("")
  const [note, setNote] = useState("")
  const [shown, setShown] = useState<typeof target>(null)
  if (target !== shown) {
    setShown(target)
    if (target) {
      setGame(target.game_key)
      setCurrency(target.currency)
      setAmount("")
      setNote("")
    }
  }
  const save = async () => {
    const delta = Math.round(Number(amount))
    if (!currency.trim() || !delta) return toast.error("Enter the currency and a non-zero amount")
    const ok = await tryAct(() => call("admin_wallet_adjust", game, userId, currency.trim(), delta, note.trim()), "Balance changed")
    if (ok !== undefined) { onDone(); onClose() }
  }
  return (
    <Dialog open={!!target} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Adjust the wallet</DialogTitle>
          <DialogDescription>Adds (or with a minus, takes) currency; a balance cannot go below zero this way. The note goes to the history and the audit log.</DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-3">
          {games.length > 1 && (
            <Field label="App">
              <select className="h-9 w-full rounded-md border bg-transparent px-2 text-sm" value={game} onChange={(e) => setGame(e.target.value)}>
                {games.map((g) => <option key={g} value={g}>{appName(g)}</option>)}
              </select>
            </Field>
          )}
          <div className="grid grid-cols-2 gap-3">
            <Field label="Currency"><Input className="font-mono text-xs" value={currency} onChange={(e) => setCurrency(e.target.value)} placeholder="gems" /></Field>
            <Field label="Amount"><Input type="number" step="1" value={amount} onChange={(e) => setAmount(e.target.value)} placeholder="100 or -50" /></Field>
          </div>
          <Field label="Note"><Textarea value={note} onChange={(e) => setNote(e.target.value)} placeholder="Compensation for ticket #123" /></Field>
        </DialogBody>
        <DialogFooter><Button onClick={save}>Save</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
