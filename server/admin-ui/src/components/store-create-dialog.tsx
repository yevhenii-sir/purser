import { useEffect, useMemo, useRef, useState } from "react"
import { CopyIcon, XIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Count, DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { call, proc, select } from "@/lib/api"
import { buildLabel, creatorBuilds, LISTING_LIMITS, storeById, storeOf } from "@/lib/stores"
import { errorText, money } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { PERIODS } from "@/lib/periods"
import { listingOf, type GameApp, type Kind, type StoreCreateJob } from "@/lib/types"

/** A product of the app to create in the stores, with the store listing saved on the product. */
export interface CreateSource {
  product_id: string
  kind: Kind
  /** The price most builds share; builds with their own price use `prices`. */
  price_micros: number
  currency: string
  /** Each build's saved price (builds with a price only). */
  prices: Record<string, { price_micros: number; currency: string }>
  title: string
  description: string
  /** The App Store builds' own title and description; empty: the shared ones. */
  apple_title?: string
  apple_description?: string
  period: string
  apple_group: string
}

interface Draft {
  on: boolean
  title: string
  description: string
  apple_title: string
  apple_description: string
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
  /** Builds the server's time budget did not reach: preview them with another call. */
  pending: string[]
}

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
  /** Product ids ticked at first (the rows picked in the table). */
  preselected?: string[]
  onClose: () => void
  onStarted: () => void
}

function StoreCreate({ apps, products, listed, preselected = [], onClose, onStarted }: StoreCreateProps) {
  const builds = creatorBuilds(apps)
  const [drafts, setDrafts] = useState<Record<string, Draft>>(() =>
    Object.fromEntries(products.map((p) => [p.product_id, {
      on: preselected.includes(p.product_id), title: p.title || titleOf(p.product_id), description: p.description,
      apple_title: p.apple_title || "", apple_description: p.apple_description || "",
      price: p.price_micros > 0 ? String(p.price_micros / 1e6) : "", currency: p.currency || "USD", period: p.period || "P1M",
    }])))
  const [targets, setTargets] = useState<Record<string, boolean>>(() => Object.fromEntries(builds.map((a) => [a.app_id, true])))
  const [group, setGroup] = useState(() => products.find((p) => p.kind === "subscription" && p.apple_group)?.apple_group || "Premium")
  const [preview, setPreview] = useState<Preview | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const set = (id: string, patch: Partial<Draft>) => setDrafts((d) => ({ ...d, [id]: { ...d[id], ...patch } }))
  const picked = products.filter((p) => drafts[p.product_id]?.on)
  const pickedBuilds = builds.filter((a) => targets[a.app_id])
  /** The text a store gets: the App Store its own where it has one. */
  const textFor = (d: Draft, store: string) => ({
    title: (store === "apple" && d.apple_title.trim()) || d.title.trim(),
    description: (store === "apple" && d.apple_description.trim()) || d.description.trim() || ((store === "apple" && d.apple_title.trim()) || d.title.trim()),
  })
  const inputs = (store = "") => picked.map((p) => {
    const d = drafts[p.product_id]
    return {
      product_id: p.product_id, kind: p.kind, ...textFor(d, store),
      price_micros: Math.round(Number(d.price || 0) * 1e6), currency: d.currency.trim().toUpperCase(),
      ...(p.kind === "subscription" ? { period: d.period, apple_group: group.trim() } : {}),
    }
  })
  /** Builds by the text they take: the App Store's, and the others' (one request each). */
  const groups = (ids: string[]) => {
    const isApple = (id: string) => storeOf(apps.find((a) => a.app_id === id)!).id === "apple"
    return ([["apple", ids.filter(isApple)], ["", ids.filter((id) => !isApple(id))]] as [string, string[]][]).filter(([, g]) => g.length)
  }
  const previewOf = async (ids: string[]): Promise<Preview> => {
    const parts = await Promise.all(groups(ids).map(([store, g]) => proc<Preview>("admin_store_create_preview", JSON.stringify({ app_ids: g, products: inputs(store) }))))
    return {
      rows: parts.flatMap((p) => p.rows),
      apple_groups: Object.assign({}, ...parts.map((p) => p.apple_groups)),
      warnings: Object.assign({}, ...parts.map((p) => p.warnings)),
      pending: parts.flatMap((p) => p.pending),
    }
  }
  /** A build's own saved price when it differs from the product's shared one, else the draft price. */
  const forBuild = <T extends { product_id: string; price_micros: number; currency: string }>(appId: string, item: T): T => {
    const p = products.find((x) => x.product_id === item.product_id)
    const own = p?.prices[appId]
    return own && (own.price_micros !== p!.price_micros || own.currency !== p!.currency) ? { ...item, ...own } : item
  }

  const check = async () => {
    setBusy(true)
    setError(null)
    try {
      setPreview(await previewOf(pickedBuilds.map((a) => a.app_id)))
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }
  /** Previews the builds the last call did not reach and adds them to the preview. */
  const checkRest = async () => {
    if (!preview?.pending.length) return
    setBusy(true)
    try {
      const more = await previewOf(preview.pending)
      setPreview({
        rows: [...preview.rows, ...more.rows],
        apple_groups: { ...preview.apple_groups, ...more.apple_groups },
        warnings: { ...preview.warnings, ...more.warnings },
        pending: more.pending,
      })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  // Builds whose jobs started in an earlier try: a retry after a failure goes on with the rest
  // (starting them again would be refused as "already being created").
  const startedBuilds = useRef(new Set<string>())
  const start = async () => {
    if (!preview) return
    setBusy(true)
    let started = 0
    try {
      for (const a of pickedBuilds) {
        if (startedBuilds.current.has(a.app_id)) continue
        const ids = new Set(preview.rows.filter((r) => r.app_id === a.app_id && r.status === "create").map((r) => r.product_id))
        const items = inputs(storeOf(a).id).filter((p) => ids.has(p.product_id)).map((p) => forBuild(a.app_id, p))
        if (!items.length) continue
        await call("admin_store_create", JSON.stringify({ app_ids: [a.app_id], products: items }))
        startedBuilds.current.add(a.app_id)
        started += items.length
      }
      toast.success(`Creating ${started} product(s); progress is shown on the Products tab`)
      onStarted()
      onClose()
    } catch (e) {
      toast.error(errorText(e))
      // Jobs of the builds before the failure are running: show them.
      if (started > 0) onStarted()
    } finally {
      setBusy(false)
    }
  }

  /** Products the store already has but this build does not list yet. */
  const existing = (preview?.rows || []).filter((r) => r.status === "exists" && r.store_product_id && !listed(r.app_id, r.store_product_id))
  const addExisting = async () => {
    const items = existing.map((r) => {
      const p = inputs(storeOf(appByid(r.app_id)!).id).find((x) => x.product_id === r.product_id)
      const price = p ? forBuild(r.app_id, p) : { price_micros: 0, currency: "" }
      return {
        app_id: r.app_id, product_id: r.store_product_id, catalog_id: r.product_id, kind: r.kind, price_micros: price.price_micros, currency: price.currency,
        ...listingOf(p),
      }
    })
    const ok = await tryAct(() => call("admin_upsert_products", JSON.stringify(items)), `Added ${items.length} product(s)`)
    if (ok !== undefined) onStarted()
  }

  const creates = preview?.rows.filter((r) => r.status === "create").length || 0
  const hasSubscription = picked.some((p) => p.kind === "subscription")
  const appleGroups = [...new Set(Object.values(preview?.apple_groups || {}).flat())]
  const stores = [...new Set(pickedBuilds.map((a) => storeOf(a).id))]
  /** Picked stores other than the App Store with a listing limit; the App Store gets its own fields. */
  const limits = stores.filter((s) => s !== "apple" && LISTING_LIMITS[s]).map((s): [string, { title: number; description: number }] => [s, LISTING_LIMITS[s]])
  const apple = stores.includes("apple")
  /** Only App Store builds picked: one field, showing the text the App Store gets (its own, else the shared one). */
  const appleOnly = apple && stores.length === 1
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
                  cell: (p) => {
                    const d = drafts[p.product_id]
                    return (
                      <div className="flex min-w-40 flex-col gap-1">
                        {!appleOnly && <Input className="h-8" value={d.title} onChange={(e) => set(p.product_id, { title: e.target.value })} />}
                        {!appleOnly && limits.map(([s, l]) => <Limit key={s} store={s} text={textFor(d, s).title} max={l.title} />)}
                        {apple && (
                          <Input className="h-8" placeholder="App Store: as above" aria-label="Title in the App Store" value={appleOnly ? d.apple_title || d.title : d.apple_title}
                            onChange={(e) => set(p.product_id, { apple_title: e.target.value })} />
                        )}
                        {apple && <Limit store="apple" text={textFor(d, "apple").title} max={LISTING_LIMITS.apple.title} />}
                      </div>
                    )
                  },
                },
                {
                  title: "Description",
                  cell: (p) => {
                    const d = drafts[p.product_id]
                    return (
                      <div className="flex min-w-40 flex-col gap-1">
                        {!appleOnly && <Input className="h-8" placeholder="Same as the title" value={d.description} onChange={(e) => set(p.product_id, { description: e.target.value })} />}
                        {!appleOnly && limits.map(([s, l]) => <Limit key={s} store={s} text={textFor(d, s).description} max={l.description} />)}
                        {apple && (
                          <Input className="h-8" placeholder={appleOnly ? "Same as the title" : "App Store: as above"} aria-label="Description in the App Store"
                            value={appleOnly ? d.apple_description || d.description : d.apple_description} onChange={(e) => set(p.product_id, { apple_description: e.target.value })} />
                        )}
                        {apple && <Limit store="apple" text={textFor(d, "apple").description} max={LISTING_LIMITS.apple.description} />}
                      </div>
                    )
                  },
                },
                {
                  title: "Price",
                  cell: (p) => (
                    <div className="space-y-1">
                      <div className="flex gap-1">
                        <Input className="h-8 w-20" type="number" step="0.01" min="0" value={drafts[p.product_id].price} onChange={(e) => set(p.product_id, { price: e.target.value })} />
                        <Input className="h-8 w-16" value={drafts[p.product_id].currency} onChange={(e) => set(p.product_id, { currency: e.target.value })} />
                      </div>
                      {pickedBuilds.filter((a) => { const own = p.prices[a.app_id]; return own && (own.price_micros !== p.price_micros || own.currency !== p.currency) }).map((a) => (
                        <div key={a.app_id} className="text-xs whitespace-nowrap text-muted-foreground">{buildLabel(a)}: {money(p.prices[a.app_id].price_micros, p.prices[a.app_id].currency)}</div>
                      ))}
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
            {preview.pending.length > 0 && (
              <div className="flex flex-wrap items-center gap-2 rounded-md border px-3 py-2 text-xs">
                <Tone tone="warn">{preview.pending.length} build(s) not checked yet</Tone>
                <span className="text-muted-foreground">The stores answered slowly; nothing is created for them until they are checked.</span>
                <Button size="xs" variant="outline" disabled={busy} onClick={checkRest}>Check them</Button>
              </div>
            )}
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
  const jobs = useAsync(() => select<StoreCreateJob>({ table: "store_create_job", where: { column: "app_id", equals: ids } }), [ids.join(",")])
  const list = useMemo(() => [...(jobs.data || [])].sort((a, b) => Number(b.id) - Number(a.id)), [jobs.data])
  const pending = list.some((j) => j.status === "pending")
  const done = list.filter((j) => j.status === "done").length
  const seenDone = useRef<number | null>(null)

  useEffect(() => {
    if (!pending) return
    // Not while the tab is hidden: the next look is when it is shown again.
    const tick = () => (document.hidden ? document.addEventListener("visibilitychange", tick, { once: true }) : jobs.reload())
    const t = setTimeout(tick, 2000)
    return () => {
      clearTimeout(t)
      document.removeEventListener("visibilitychange", tick)
    }
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

/** The length a store takes, under a field: `Google Play 40/55`, red when longer. */
function Limit({ store, text, max }: { store: string; text: string; max: number }) {
  return <div className="text-[11px] text-muted-foreground">{storeById(store)?.name || store} <Count text={text} max={max} /></div>
}
