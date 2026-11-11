import { useEffect, useMemo, useRef, useState } from "react"
import { CopyIcon, XIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { call, proc, sqlWhereIn } from "@/lib/api"
import { buildLabel, creatorBuilds, storeOf } from "@/lib/stores"
import { errorText } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import type { GameApp, Kind, StoreCreateJob } from "@/lib/types"

/** A product of the app to create in the stores. */
export interface CreateSource {
  product_id: string
  kind: Kind
  price_micros: number
  currency: string
}

interface Draft {
  on: boolean
  title: string
  description: string
  price: string
  currency: string
  period: string
}

interface PreviewRow {
  app_id: string
  store: string
  product_id: string
  kind: Kind
  status: "create" | "exists" | "unsupported" | "invalid"
  reason: string
  store_product_id: string
}

interface Preview {
  rows: PreviewRow[]
  apple_groups: Record<string, string[]>
  warnings: Record<string, string>
}

const PERIODS: [string, string][] = [["P1W", "Weekly"], ["P1M", "Monthly"], ["P3M", "Every 3 months"], ["P6M", "Every 6 months"], ["P1Y", "Yearly"]]

/** What each store needs before products can be created there. */
const CREATE_HELP: Record<string, string> = {
  google: 'Google Play: the service account needs "Manage store presence", and the app needs a build with Play Billing uploaded to any track.',
  apple: "App Store: needs the App Store Connect API Team key (App Manager role). Your first in-app purchase must be submitted for review together with an app version; the review screenshot is added in App Store Connect.",
  stripe: "Stripe: a restricted key needs Products: Write and Prices: Write. The new Price id becomes this build's product id: put it into the Unity config's store ids.",
  xsolla: "Xsolla: virtual items are created with the project API key; subscriptions are not supported.",
}

const titleOf = (id: string) => id.replace(/[_.-]+/g, " ").replace(/^\w/, (c) => c.toUpperCase())

const statusTone: Record<PreviewRow["status"], "ok" | "muted" | "warn" | "bad"> = { create: "ok", exists: "muted", unsupported: "warn", invalid: "bad" }

export function StoreCreateDialog({ open, ...props }: StoreCreateProps & { open: boolean }) {
  return (
    <Dialog open={open} onOpenChange={(o) => !o && props.onClose()}>
      <DialogContent className="sm:max-w-4xl">{open && <StoreCreate {...props} />}</DialogContent>
    </Dialog>
  )
}

interface StoreCreateProps {
  apps: GameApp[]
  products: CreateSource[]
  /** The build already lists this product id. */
  listed: (appId: string, productId: string) => boolean
  onClose: () => void
  onStarted: () => void
}

function StoreCreate({ apps, products, listed, onClose, onStarted }: StoreCreateProps) {
  const builds = creatorBuilds(apps)
  const [drafts, setDrafts] = useState<Record<string, Draft>>(() =>
    Object.fromEntries(products.map((p) => [p.product_id, {
      on: false, title: titleOf(p.product_id), description: "",
      price: p.price_micros > 0 ? String(p.price_micros / 1e6) : "", currency: p.currency || "USD", period: "P1M",
    }])))
  const [targets, setTargets] = useState<Record<string, boolean>>(() => Object.fromEntries(builds.map((a) => [a.app_id, true])))
  const [group, setGroup] = useState("Premium")
  const [preview, setPreview] = useState<Preview | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const set = (id: string, patch: Partial<Draft>) => setDrafts((d) => ({ ...d, [id]: { ...d[id], ...patch } }))
  const picked = products.filter((p) => drafts[p.product_id]?.on)
  const pickedBuilds = builds.filter((a) => targets[a.app_id])
  const inputs = () => picked.map((p) => {
    const d = drafts[p.product_id]
    return {
      product_id: p.product_id, kind: p.kind, title: d.title.trim(), description: d.description.trim(),
      price_micros: Math.round(Number(d.price || 0) * 1e6), currency: d.currency.trim().toUpperCase(),
      ...(p.kind === "subscription" ? { period: d.period, apple_group: group.trim() } : {}),
    }
  })
  const request = () => JSON.stringify({ app_ids: pickedBuilds.map((a) => a.app_id), products: inputs() })

  const check = async () => {
    setBusy(true)
    setError(null)
    try {
      setPreview(await proc<Preview>("admin_store_create_preview", request()))
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const start = async () => {
    if (!preview) return
    setBusy(true)
    const all = inputs()
    let started = 0
    try {
      for (const a of pickedBuilds) {
        const ids = new Set(preview.rows.filter((r) => r.app_id === a.app_id && r.status === "create").map((r) => r.product_id))
        const items = all.filter((p) => ids.has(p.product_id))
        if (!items.length) continue
        await call("admin_store_create", JSON.stringify({ app_ids: [a.app_id], products: items }))
        started += items.length
      }
      toast.success(`Creating ${started} product(s); progress is shown on the Products tab`)
      onStarted()
      onClose()
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  /** Products the store already has but this build does not list yet. */
  const existing = (preview?.rows || []).filter((r) => r.status === "exists" && r.store_product_id && !listed(r.app_id, r.store_product_id))
  const addExisting = async () => {
    const byId = new Map(inputs().map((p) => [p.product_id, p]))
    const items = existing.map((r) => {
      const p = byId.get(r.product_id)
      return { app_id: r.app_id, product_id: r.store_product_id, kind: r.kind, default_price_micros: p?.price_micros || 0, currency: p?.currency || "" }
    })
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Added ${items.length} product(s)`)
    if (ok !== undefined) onStarted()
  }

  const creates = preview?.rows.filter((r) => r.status === "create").length || 0
  const hasSubscription = picked.some((p) => p.kind === "subscription")
  const appleGroups = [...new Set(Object.values(preview?.apple_groups || {}).flat())]
  const stores = [...new Set(pickedBuilds.map((a) => storeOf(a).id))]
  const appByid = (id: string) => apps.find((a) => a.app_id === id)

  return (
    <>
      <DialogHeader>
        <DialogTitle>Create in store</DialogTitle>
        <DialogDescription>
          Creates the picked products in the stores of the picked builds, with an English title and description and the price converted to every
          country by the store. Products the store already has are skipped. Each store needs a key that may change its catalog.
        </DialogDescription>
      </DialogHeader>
      <DialogBody>
        {builds.length === 0 ? (
          <p className="text-sm text-muted-foreground">None of this app's builds is in a store whose products can be created from here (Google Play, App Store, Stripe, Xsolla).</p>
        ) : !preview ? (
          <div className="space-y-4">
            <div className="flex flex-wrap gap-6">
              {builds.map((a) => (
                <div key={a.app_id} className="flex items-center gap-2">
                  <Checkbox id={`target-${a.app_id}`} checked={!!targets[a.app_id]} onCheckedChange={(v) => setTargets((t) => ({ ...t, [a.app_id]: v === true }))} />
                  <Label htmlFor={`target-${a.app_id}`} className="font-normal">{buildLabel(a)}</Label>
                </div>
              ))}
            </div>
            <DataTable
              rows={products}
              rowKey={(p) => p.product_id}
              empty="This app has no products yet: add them (or import them from Unity) first."
              columns={[
                {
                  title: "",
                  className: "w-8",
                  cell: (p) => <Checkbox checked={drafts[p.product_id].on} onCheckedChange={(v) => set(p.product_id, { on: v === true })} aria-label={`Create ${p.product_id}`} />,
                },
                { title: "Product", cell: (p) => <div className="min-w-0"><Mono>{p.product_id}</Mono><div className="text-xs text-muted-foreground">{p.kind.replace("_", "-")}</div></div> },
                {
                  title: "Title",
                  cell: (p) => <Input className="h-8 min-w-40" value={drafts[p.product_id].title} onChange={(e) => set(p.product_id, { title: e.target.value })} />,
                },
                {
                  title: "Description",
                  cell: (p) => <Input className="h-8 min-w-40" placeholder="Same as the title" value={drafts[p.product_id].description}
                    onChange={(e) => set(p.product_id, { description: e.target.value })} />,
                },
                {
                  title: "Price",
                  cell: (p) => (
                    <div className="flex gap-1">
                      <Input className="h-8 w-20" type="number" step="0.01" min="0" value={drafts[p.product_id].price} onChange={(e) => set(p.product_id, { price: e.target.value })} />
                      <Input className="h-8 w-16" value={drafts[p.product_id].currency} onChange={(e) => set(p.product_id, { currency: e.target.value })} />
                    </div>
                  ),
                },
                {
                  title: "Period",
                  cell: (p) => p.kind !== "subscription" ? <span className="text-muted-foreground">—</span> : (
                    <Select value={drafts[p.product_id].period} onValueChange={(v) => set(p.product_id, { period: v })}>
                      <SelectTrigger className="h-8 w-36"><SelectValue /></SelectTrigger>
                      <SelectContent>{PERIODS.map(([v, l]) => <SelectItem key={v} value={v}>{l}</SelectItem>)}</SelectContent>
                    </Select>
                  ),
                },
              ]}
            />
            {hasSubscription && stores.includes("apple") && (
              <Field label="App Store subscription group" className="max-w-sm">
                <Input value={group} onChange={(e) => setGroup(e.target.value)} />
              </Field>
            )}
            {stores.length > 0 && (
              <ul className="list-disc space-y-1 pl-5 text-xs text-muted-foreground">
                {stores.map((s) => CREATE_HELP[s] && <li key={s}>{CREATE_HELP[s]}</li>)}
              </ul>
            )}
            {error && <ErrorBox error={error} />}
          </div>
        ) : (
          <div className="space-y-3">
            {Object.entries(preview.warnings).map(([app, w]) => (
              <p key={app} className="rounded-md border px-3 py-2 text-xs break-words">
                <Tone tone="warn">{appByid(app) ? buildLabel(appByid(app)!) : app}</Tone> {w}, so products it already has are not detected.
              </p>
            ))}
            <DataTable
              rows={preview.rows}
              rowKey={(r) => `${r.app_id}/${r.product_id}`}
              columns={[
                { title: "Build", cell: (r) => (appByid(r.app_id) ? buildLabel(appByid(r.app_id)!) : r.app_id) },
                { title: "Product", cell: (r) => <Mono>{r.product_id}</Mono> },
                { title: "Result", cell: (r) => <Tone tone={statusTone[r.status]}>{r.status === "exists" ? "already in the store" : r.status === "create" ? "will be created" : r.status}</Tone> },
                {
                  title: "",
                  cell: (r) => <span className="text-xs text-muted-foreground">{r.reason || (r.store_product_id && r.store_product_id !== r.product_id ? r.store_product_id : "")}</span>,
                },
              ]}
            />
            {hasSubscription && appleGroups.length > 0 && (
              <p className="text-xs text-muted-foreground">
                App Store subscription groups: {appleGroups.join(", ")}. Subscriptions go to <b>{group}</b>{appleGroups.some((g) => g.toLowerCase() === group.trim().toLowerCase()) ? "" : " (a new group)"}.
              </p>
            )}
            {existing.length > 0 && (
              <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                <span>{existing.length} product(s) already exist in the store.</span>
                <Button variant="outline" size="sm" onClick={addExisting}>Add them to this app's products</Button>
              </div>
            )}
          </div>
        )}
      </DialogBody>
      <DialogFooter className="gap-2">
        {preview ? (
          <>
            <Button variant="outline" onClick={() => setPreview(null)} disabled={busy}>Back</Button>
            <Button onClick={start} disabled={busy || creates === 0}>{creates ? `Create ${creates}` : "Nothing to create"}</Button>
          </>
        ) : (
          <Button onClick={check} disabled={busy || !picked.length || !pickedBuilds.length}>{busy ? "Checking…" : "Check"}</Button>
        )}
      </DialogFooter>
    </>
  )
}

const jobTone = { pending: "warn", done: "ok", failed: "bad" } as const

/** Products being created in the stores (and the results until dismissed), refreshed while any is running. */
export function StoreJobs({ apps, onFinished }: { apps: GameApp[]; onFinished: () => void }) {
  const ids = apps.map((a) => a.app_id)
  const jobs = useAsync(() => sqlWhereIn<StoreCreateJob>("store_create_job", "app_id", ids), [ids.join(",")])
  const list = useMemo(() => [...(jobs.data || [])].sort((a, b) => Number(b.id) - Number(a.id)), [jobs.data])
  const pending = list.some((j) => j.status === "pending")
  const done = list.filter((j) => j.status === "done").length
  const seenDone = useRef<number | null>(null)

  useEffect(() => {
    if (!pending) return
    const t = setTimeout(jobs.reload, 2000)
    return () => clearTimeout(t)
  }, [pending, jobs.data, jobs.reload])
  useEffect(() => {
    if (seenDone.current !== null && done > seenDone.current) onFinished()
    seenDone.current = done
  }, [done, onFinished])

  const dismiss = async (ids: number[]) => {
    await tryAct(async () => {
      for (const id of ids) await call("admin_dismiss_store_job", id)
    })
    jobs.reload()
  }
  if (!list.length) return null
  const appOf = (id: string) => apps.find((a) => a.app_id === id)
  const finished = list.filter((j) => j.status !== "pending").map((j) => Number(j.id))
  return (
    <Section
      title="Created in stores"
      description="Products being created in the stores. Finished ones are added to the build's products."
      actions={finished.length > 0 && <Button variant="outline" size="sm" onClick={() => dismiss(finished)}>Clear finished</Button>}
    >
      {jobs.error ? <ErrorBox error={jobs.error} /> : jobs.loading && !jobs.data ? <Loading /> : (
        <DataTable
          rows={list}
          rowKey={(j) => String(j.id)}
          columns={[
            { title: "Product", cell: (j) => <Mono>{j.product_id}</Mono> },
            { title: "Build", cell: (j) => (appOf(j.app_id) ? buildLabel(appOf(j.app_id)!) : j.app_id) },
            { title: "Status", cell: (j) => <Tone tone={jobTone[j.status]}>{j.status === "pending" ? j.step || "queued" : j.status}</Tone> },
            {
              title: "Details",
              cell: (j) => (
                <div className="min-w-0 max-w-xl space-y-0.5 text-xs break-words whitespace-normal">
                  {j.error && <div className={j.status === "failed" ? "text-destructive" : "text-muted-foreground"}>{j.error}</div>}
                  {j.note && <div className="text-muted-foreground">{j.note}</div>}
                  {j.status === "done" && j.store_product_id !== j.product_id && (
                    <div className="flex items-center gap-1">
                      <span className="text-muted-foreground">Store id</span> <Mono>{j.store_product_id}</Mono>
                      <Button variant="ghost" size="icon" className="size-6" aria-label="Copy the store id"
                        onClick={() => tryAct(() => navigator.clipboard.writeText(j.store_product_id), "Copied")}><CopyIcon /></Button>
                    </div>
                  )}
                </div>
              ),
            },
            {
              title: "",
              className: "w-8",
              cell: (j) => (
                <Button variant="ghost" size="icon" className="size-7" aria-label={j.status === "pending" ? "Stop" : "Dismiss"} title={j.status === "pending" ? "Stop" : "Dismiss"}
                  onClick={() => dismiss([Number(j.id)])}><XIcon /></Button>
              ),
            },
          ]}
        />
      )}
    </Section>
  )
}
