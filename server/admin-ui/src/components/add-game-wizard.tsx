import { useCallback, useRef, useState, type ReactNode } from "react"
import { CheckIcon, ClipboardPasteIcon, DownloadIcon, PlusIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Separator } from "@/components/ui/separator"
import { Textarea } from "@/components/ui/textarea"
import { AccountSelect, accountsOf } from "@/components/account-select"
import { CatalogPicker, type PickedProduct } from "@/components/catalog-picker"
import { CopyField, DialogBody, ErrorBox, Field, Loading, Tone } from "@/components/common"
import { ReportView, reportUrls } from "@/components/report"
import { call, createIdentity as newIdentity, genSecret, proc, publicRoute, sql, sqlStr } from "@/lib/api"
import { appsChanged, useApps, type AppGroup } from "@/lib/apps"
import { checkBuilds } from "@/lib/checks"
import { buildLabel, platformById, storeById, storeOf, storesFor, usablePlatforms, type StoreInfo } from "@/lib/stores"
import { errorText } from "@/lib/format"
import { parseUnityConfig, productIdIn, suggestGameKey, toUnityConfig, type ParsedUnityConfig } from "@/lib/unity-config"
import type { CatalogRow, Delivery, GameApp, Kind, Platform, Product, Report } from "@/lib/types"
import { catalogIdOf, KINDS } from "@/lib/types"
import { cn } from "@/lib/utils"

const STEPS = ["App", "Delivery", "Store keys", "Products", "Check"]

function reducerSnippet(identity: string) {
  return `// Rust module of the app database: the billing calls these reducers (args_format v1)
const BILLING_IDENTITY_HEX: &str = "${identity || "<billing identity>"}";

#[spacetimedb::reducer]
pub fn billing_grant(ctx: &ReducerContext, ev: BillingEvent) -> Result<(), String> {
    if ctx.sender() != Identity::from_hex(BILLING_IDENTITY_HEX).unwrap() { return Err("unauthorized".into()); }
    // idempotent: apply each (ev.transaction_id, ev.event) once; owned products as state
    Ok(())
}
// billing_revoke: same signature; BillingEvent: see docs/game-integration.md`
}

/** One build of the app: an app id in one store, published from a developer account. */
interface Build {
  app_id: string
  platform: Platform
  /** Store id (registry); fixed by the platform unless it has several (web). */
  store: string
  account: string
  /** Receives the products of step 4. */
  products: boolean
}

function newBuild(platform: string, account = "default"): Build {
  return { app_id: "", platform: platform as Platform, store: storesFor(platform)[0]?.id || "", account, products: true }
}

interface WizardState {
  game_key: string
  /** Rows without an app id are ignored. */
  builds: Build[]
  delivery: Delivery
  spacetime_url: string
  spacetime_db: string
  grant_reducer: string
  revoke_reducer: string
  args_format: string
  webhook_url: string
  identity: string
  products: PickedProduct[]
}

const INITIAL: WizardState = {
  game_key: "", builds: [newBuild("android"), newBuild("ios")], delivery: "spacetimedb",
  spacetime_url: "https://maincloud.spacetimedb.com", spacetime_db: "", grant_reducer: "billing_grant", revoke_reducer: "billing_revoke",
  args_format: "v1", webhook_url: "", identity: "", products: [],
}

/** Store keys entered in step 3, per store and developer account. */
const keyId = (store: string, account: string, name: string) => `${store}|${account}|${name}`

/** File upload into a text secret (JSON key, .p8). */
function SecretInput({ label, value, onChange, isSet, file, hint }: {
  label: string
  value: string
  onChange: (v: string) => void
  isSet: boolean
  file?: boolean
  hint?: ReactNode
}) {
  return (
    <Field
      label={<span className="flex items-center gap-2">{label}{isSet && <Tone tone="ok">set, leave empty to keep</Tone>}</span>}
      hint={hint}
    >
      {file ? (
        <>
          <Textarea value={value} onChange={(e) => onChange(e.target.value)} autoComplete="off" spellCheck={false} className="min-h-20 font-mono text-xs" />
          <Input type="file" className="text-xs" onChange={async (e) => { const f = e.target.files?.[0]; if (f) onChange(await f.text()) }} />
        </>
      ) : (
        <Input value={value} onChange={(e) => onChange(e.target.value)} autoComplete="off" spellCheck={false} />
      )}
    </Field>
  )
}

/** `existing`: run the wizard again on an app (e.g. to add a build). `onClose(key, changed)` gets
 * the app key when the wizard was finished, and `changed` when any step saved something. */
export function AddGameWizard({ open, existing, onClose }: { open: boolean; existing?: AppGroup; onClose: (key?: string, changed?: boolean) => void }) {
  const changed = useRef(false)
  const close = (key?: string) => {
    onClose(key, changed.current)
    changed.current = false
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !o && close()}>
      <DialogContent className="sm:max-w-3xl">{open && <Wizard existing={existing} onClose={close} onChange={() => { changed.current = true }} />}</DialogContent>
    </Dialog>
  )
}

function Wizard({ existing, onClose, onChange }: { existing?: AppGroup; onClose: (key?: string) => void; onChange: () => void }) {
  const [step, setStep] = useState(0)
  const [w, setW] = useState<WizardState>(() =>
    existing
      ? {
          ...INITIAL, game_key: existing.key,
          builds: existing.apps.map((a) => ({ app_id: a.app_id, platform: a.platform, store: storeOf(a).id, account: a.account || "default", products: true })),
        }
      : INITIAL)
  const [busy, setBusy] = useState(false)
  const [creds, setCreds] = useState<Set<string> | null>(null)
  const [token, setToken] = useState("")
  const [keys, setKeys] = useState<Record<string, string>>({})
  const [generated, setGenerated] = useState<Record<string, string>>({})
  const [catalog, setCatalog] = useState<CatalogRow[] | null>(null)
  const [catalogPick, setCatalogPick] = useState<PickedProduct[]>([])
  const [idHint, setIdHint] = useState<ReactNode>(null)
  const [reports, setReports] = useState<{ reports: Report[]; base: string } | null>(null)

  const set = <K extends keyof WizardState>(k: K, v: WizardState[K]) => setW((s) => ({ ...s, [k]: v }))
  const apps = (s = w) => s.builds.filter((b) => b.app_id.trim())
  const known = useApps().apps
  const allAccounts = accountsOf(known || [])
  const storeOfBuild = (b: Build): StoreInfo => storeById(b.store) || storeOf(b)
  const has = (name: string, b: Build) => !!creds && [b.app_id, b.account || "default", "default"].some((s) => creds.has(`${s}/${name}`))
  const setBuild = (i: number, patch: Partial<Build>) => setW((s) => ({ ...s, builds: s.builds.map((b, j) => (j === i ? { ...b, ...patch } : b)) }))
  /** Store keys to ask for: one group per store and developer account. */
  const keyGroups = () => {
    const out: { store: StoreInfo; account: string; builds: Build[] }[] = []
    for (const b of apps()) {
      const store = storeOfBuild(b)
      const g = out.find((x) => x.store.id === store.id && x.account === b.account)
      if (g) g.builds.push(b)
      else out.push({ store, account: b.account || "default", builds: [b] })
    }
    return out
  }
  const onCatalogPick = useCallback((p: PickedProduct[]) => setCatalogPick(p), [])

  const saveSecret = async (scope: string, name: string, value: string) => {
    if (!value.trim()) return
    onChange()
    await call("admin_set_credential", scope, name, value.trim())
    setCreds((c) => new Set([...(c || []), `${scope}/${name}`]))
  }

  /** Checks every build at once. `appleTest`: also ask Apple for a test notification (App Store builds). */
  const runChecks = async (s = w, appleTest = false) => {
    setReports(null)
    onChange()
    const ids = apps(s).filter((b) => !appleTest || storeOfBuild(b).id === "apple").map((b) => b.app_id)
    const [base, out] = await Promise.all([publicRoute(), checkBuilds(ids, appleTest)])
    // An Apple test only re-checks the App Store builds: keep the other reports.
    const merged = appleTest && reports ? reports.reports.map((r) => out.find((o) => o.app_id === r.app_id) || r) : out
    setReports({ reports: merged, base })
  }
  const hasAppleBuild = apps().some((b) => storeOfBuild(b).id === "apple")

  const next = async () => {
    setBusy(true)
    try {
      if (step === 0) {
        const builds = apps()
        if (!builds.length) throw new Error("Enter the app id of at least one build")
        const ids = builds.map((b) => b.app_id.trim())
        const dup = ids.find((id, i) => ids.indexOf(id) !== i)
        if (dup) throw new Error(`${dup} is used by two builds: give each build its own app id (e.g. ${dup}.amazon for the Amazon Appstore build)`)
        if (ids.some((id) => /[/:]/.test(id))) throw new Error("App ids must not contain '/' or ':'")
        for (const b of builds) if (!b.store) throw new Error(`Pick a store for the ${platformById(b.platform)?.name || b.platform} build`)
        const s = { ...w, builds: w.builds.map((b) => ({ ...b, app_id: b.app_id.trim(), account: b.account || "default" })), game_key: w.game_key || ids[0] }
        const firstApp = apps(s)[0]?.app_id || ""
        const [products, credRows] = await Promise.all([
          s.products.length ? Promise.resolve([] as Product[]) : sql<Product>(`SELECT * FROM product WHERE app_id = ${sqlStr(firstApp)}`),
          sql<{ key: string }>("SELECT key FROM credential"),
        ])
        const existing: GameApp[] = known || []
        setCreds(new Set(credRows.map((c) => c.key)))
        for (const a of apps(s)) {
          const e = existing.find((x) => x.app_id === a.app_id)
          if (e) Object.assign(s, { delivery: e.delivery, spacetime_url: e.spacetime_url || s.spacetime_url, spacetime_db: e.spacetime_db, grant_reducer: e.grant_reducer, revoke_reducer: e.revoke_reducer, args_format: e.args_format, webhook_url: e.webhook_url })
        }
        if (!s.products.length) {
          const firstStore = apps(s)[0]?.store || ""
          s.products = products
            .filter((p) => p.app_id === firstApp)
            .map((p) => ({
              product_id: catalogIdOf(p), kind: p.kind, default_price_micros: Number(p.default_price_micros), currency: p.currency,
              ...(p.product_id !== catalogIdOf(p) ? { store_ids: { [firstStore]: p.product_id } } : {}),
              ...(p.period ? { period: p.period } : {}),
            }))
        }
        setW(s)
      } else if (step === 1) {
        // Delivery keys are needed by every build: store them for each build's account.
        const buildAccounts = [...new Set(apps().map((a) => a.account || "default"))]
        onChange()
        if (w.delivery === "spacetimedb") for (const acc of buildAccounts) await saveSecret(acc, "spacetime_token", token)
        for (const a of apps()) {
          await call("admin_upsert_game_app", JSON.stringify({
            app_id: a.app_id, platform: a.platform, store: a.store, game_key: w.game_key, account: a.account || "default", delivery: w.delivery,
            spacetime_url: w.delivery === "spacetimedb" ? w.spacetime_url : "", spacetime_db: w.spacetime_db,
            grant_reducer: w.grant_reducer, revoke_reducer: w.revoke_reducer, args_format: w.args_format, webhook_url: w.webhook_url,
          }))
        }
        appsChanged()
        if (w.delivery === "webhook" && !has("webhook_secret", apps()[0])) {
          const secret = genSecret()
          for (const acc of buildAccounts) await saveSecret(acc, "webhook_secret", secret)
          setGenerated((g) => ({ ...g, webhook_secret: secret }))
        }
      } else if (step === 2) {
        // Account-wide store keys go to the developer account; a build's own keys to the build.
        for (const g of keyGroups()) {
          for (const spec of g.store.secrets.filter((x) => x.need !== "optional")) {
            const value = keys[keyId(g.store.id, g.account, spec.name)] || ""
            if (spec.name.endsWith("_json") && value.trim()) {
              try { JSON.parse(value) } catch { throw new Error(`${spec.label} is not JSON`) }
            }
            if (spec.account_wide) await saveSecret(g.account, spec.name, value)
            else if (spec.name !== g.store.notify_secret) for (const b of g.builds) await saveSecret(b.app_id, spec.name, value)
          }
        }
        // Notification URL secrets are generated per build and shown once on the last step.
        for (const b of apps()) {
          const store = storeOfBuild(b)
          const spec = store.secrets.find((x) => x.name === store.notify_secret)
          if (spec && spec.need !== "optional" && !has(spec.name, b)) {
            const secret = genSecret()
            await saveSecret(b.app_id, spec.name, secret)
            setGenerated((g) => ({ ...g, [spec.name]: secret }))
          }
        }
      } else if (step === 3) {
        const valid = w.products.filter((p) => p.product_id.trim())
        const items = apps()
          .filter((a) => a.products)
          .flatMap((a) => valid.map((p) => ({
            app_id: a.app_id, product_id: productIdIn(p, a.store).trim(), catalog_id: p.product_id.trim(), kind: p.kind, default_price_micros: p.default_price_micros || 0, currency: p.currency || "",
            ...(p.period && p.kind === "subscription" ? { period: p.period } : {}),
          })))
        if (items.length) {
          onChange()
          await call("admin_upsert_products", JSON.stringify(items))
        }
        void runChecks()
      }
      setStep((x) => x + 1)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const createIdentity = async () => {
    let origin = ""
    try { origin = new URL(w.spacetime_url).origin } catch { /* invalid URL */ }
    if (origin !== location.origin) {
      setIdHint(<>This page can only call its own server. Run <code>curl -X POST {w.spacetime_url}/v1/identity</code> and paste the <code>token</code> above.</>)
      return
    }
    try {
      const r = await newIdentity()
      setToken(r.token)
      set("identity", r.identity)
      setIdHint(`Created ${String(r.identity).slice(0, 16)}…, saved with the next step.`)
    } catch (e) {
      toast.error(errorText(e))
    }
  }

  const updateProduct = (i: number, patch: Partial<PickedProduct>) =>
    setW((s) => ({ ...s, products: s.products.map((p, j) => (j === i ? { ...p, ...patch } : p)) }))

  /** Builds whose store catalog can be imported (Google Play, App Store). */
  const importers = apps().filter((b) => storeOfBuild(b).product_import)
  const loadCatalog = async (importer: Build) => {
    setCatalog([])
    try {
      const r = await proc<{ products: CatalogRow[] }>("admin_list_store_products", importer.app_id)
      setCatalog(r.products)
    } catch (e) {
      setCatalog(null)
      toast.error(errorText(e))
    }
  }

  const takeCatalog = () => {
    setW((s) => {
      const products = [...s.products]
      for (const p of catalogPick) {
        const i = products.findIndex((x) => x.product_id === p.product_id)
        if (i >= 0) products[i] = { ...products[i], ...p }
        else products.push(p)
      }
      return { ...s, products }
    })
    setCatalog(null)
  }

  const unityConfig = reports ? toUnityConfig(reports.base, apps(), w.products) : ""

  /** A pasted Unity config: builds merged by store and app id, products by id (keeping prices). */
  const applyUnity = (c: ParsedUnityConfig) => {
    setW((s) => {
      let builds = [...s.builds]
      for (const u of c.builds) {
        if (builds.some((b) => b.store === u.store && b.app_id === u.app_id)) continue
        const empty = builds.findIndex((b) => !b.app_id.trim() && b.store === u.store)
        if (empty >= 0) builds[empty] = { ...builds[empty], app_id: u.app_id }
        else builds.push({ ...newBuild(u.platform), store: u.store, app_id: u.app_id })
      }
      if (c.builds.length) builds = builds.filter((b) => b.app_id.trim())
      const products = [...s.products]
      for (const u of c.products) {
        const i = products.findIndex((p) => p.product_id === u.product_id)
        // A price already entered here wins; the Unity fallback price fills the gaps.
        const priced = (p: PickedProduct) => (p.default_price_micros ? p : { ...p, default_price_micros: u.price_micros, currency: u.currency })
        const period = u.period ? { period: u.period } : {}
        if (i >= 0) products[i] = priced({ ...period, ...products[i], kind: u.kind, store_ids: u.store_ids })
        else products.push({ product_id: u.product_id, kind: u.kind, store_ids: u.store_ids, default_price_micros: u.price_micros, currency: u.currency, ...period })
      }
      const game_key = s.game_key || suggestGameKey(builds.map((b) => b.app_id))
      return { ...s, builds, products, game_key }
    })
    toast.success(`Pasted ${c.builds.length} build(s) and ${c.products.length} product(s)`)
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>{existing ? `Set up ${existing.name}` : "Add an app"}</DialogTitle>
        <DialogDescription>An app is one product with any number of builds: Google Play, App Store, Amazon Appstore, ... Every step saves what it collected, so the wizard can be run again.</DialogDescription>
      </DialogHeader>
      <ol className="flex flex-wrap gap-x-4 gap-y-2 text-sm">
        {STEPS.map((s, i) => (
          <li key={s} className={cn("flex items-center gap-2", i === step ? "font-medium" : "text-muted-foreground")}>
            <span className={cn("flex size-6 shrink-0 items-center justify-center rounded-full border text-xs",
              i < step && "border-primary bg-primary text-primary-foreground", i === step && "border-primary text-primary")}>
              {i < step ? <CheckIcon className="size-3.5" /> : i + 1}
            </span>
            {s}
          </li>
        ))}
      </ol>
      <Separator />
      <DialogBody>
        <div className="space-y-4 pb-1">
          {step === 0 && (
            <>
              <Field label="App key (shared by all its builds: entitlements are per app)">
                <Input value={w.game_key} onChange={(e) => set("game_key", e.target.value)} placeholder="my-game" readOnly={!!existing} />
              </Field>
              <div className="space-y-3">
                {w.builds.map((b, i) => {
                  const choices = storesFor(b.platform)
                  const store = storeOfBuild(b)
                  return (
                    <div key={i} className="grid gap-3 rounded-lg border p-3 sm:grid-cols-[10rem_1fr_12rem_auto] sm:items-end">
                      <Field label="Platform">
                        <Select value={b.platform} onValueChange={(v) => setBuild(i, { platform: v as Platform, store: storesFor(v)[0]?.id || "" })}>
                          <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                          <SelectContent>{usablePlatforms().map((p) => <SelectItem key={p.id} value={p.id}>{p.name}</SelectItem>)}</SelectContent>
                        </Select>
                        {choices.length > 1 && (
                          <Select value={b.store} onValueChange={(v) => setBuild(i, { store: v })}>
                            <SelectTrigger className="w-full"><SelectValue placeholder="Store" /></SelectTrigger>
                            <SelectContent>{choices.map((c) => <SelectItem key={c.id} value={c.id}>{c.name}</SelectItem>)}</SelectContent>
                          </Select>
                        )}
                      </Field>
                      <Field label={`${store.name}: ${store.app_id_label}`}>
                        <Input value={b.app_id} onChange={(e) => setBuild(i, { app_id: e.target.value.trim() })} placeholder={store.app_id_placeholder} />
                      </Field>
                      <Field label="Developer account">
                        <AccountSelect value={b.account} accounts={allAccounts} onChange={(v) => setBuild(i, { account: v })} />
                      </Field>
                      <Button variant="ghost" size="icon" aria-label="Remove build" disabled={!!existing?.apps.some((a) => a.app_id === b.app_id)}
                        onClick={() => setW((s) => ({ ...s, builds: s.builds.filter((_, j) => j !== i) }))}>
                        <Trash2Icon />
                      </Button>
                    </div>
                  )
                })}
              </div>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" size="sm" onClick={() => setW((s) => ({ ...s, builds: [...s.builds, newBuild("android", s.builds[0]?.account)] }))}>
                  <PlusIcon /> Add a build
                </Button>
                <UnityPaste onApply={applyUnity} />
              </div>
              <p className="text-sm text-muted-foreground">
                Empty rows are skipped. Store keys are saved for the developer account, so the next app from the same account reuses them.
                A game on Google Play and the Amazon Appstore is two builds; give the Amazon build its own app id when the package name is the same.
              </p>
            </>
          )}

          {step === 1 && (
            <>
              <Field label="Reward delivery">
                <Select value={w.delivery} onValueChange={(v) => set("delivery", v as Delivery)}>
                  <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="spacetimedb">Reducers in the app's SpacetimeDB database</SelectItem>
                    <SelectItem value="webhook">Webhook to the app backend</SelectItem>
                    <SelectItem value="none">No backend: the client grants rewards</SelectItem>
                  </SelectContent>
                </Select>
              </Field>
              {w.delivery === "spacetimedb" && (
                <>
                  <div className="grid gap-4 sm:grid-cols-2">
                    <Field label="SpacetimeDB server (https, allowed host)"><Input value={w.spacetime_url} onChange={(e) => set("spacetime_url", e.target.value)} /></Field>
                    <Field label="Database name"><Input value={w.spacetime_db} onChange={(e) => set("spacetime_db", e.target.value)} /></Field>
                    <Field label="Grant reducer"><Input value={w.grant_reducer} onChange={(e) => set("grant_reducer", e.target.value)} /></Field>
                    <Field label="Revoke reducer"><Input value={w.revoke_reducer} onChange={(e) => set("revoke_reducer", e.target.value)} /></Field>
                  </div>
                  <details className="text-sm">
                    <summary className="cursor-pointer text-muted-foreground">Advanced</summary>
                    <div className="mt-3 grid gap-2 sm:w-64">
                      <Field label="Arguments format" hint="legacy: only for games migrated from Adapty">
                        <Select value={w.args_format} onValueChange={(v) => set("args_format", v)}>
                          <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                          <SelectContent><SelectItem value="v1">v1</SelectItem><SelectItem value="legacy">legacy</SelectItem></SelectContent>
                        </Select>
                      </Field>
                    </div>
                  </details>
                  <SecretInput label="Billing token (the identity the app database trusts)" value={token} onChange={setToken}
                    isSet={!!apps()[0] && has("spacetime_token", apps()[0])} hint={idHint} />
                  <Button variant="outline" size="sm" onClick={createIdentity}><PlusIcon /> Create billing identity</Button>
                  <pre className="overflow-x-auto rounded-md bg-muted p-3 font-mono text-xs">{reducerSnippet(w.identity)}</pre>
                </>
              )}
              {w.delivery === "webhook" && (
                <>
                  <Field label="Webhook URL (https)"><Input value={w.webhook_url} onChange={(e) => set("webhook_url", e.target.value)} /></Field>
                  <p className="text-sm text-muted-foreground">
                    A new HMAC key is generated and shown on the last step unless one is already set. Verify <code>X-Billing-Signature</code> and answer 2xx to unknown events.
                  </p>
                </>
              )}
              {w.delivery === "none" && (
                <p className="text-sm text-muted-foreground">The server verifies purchases and keeps entitlements; the app grants rewards on <code>PurchaseSucceeded</code>.</p>
              )}
            </>
          )}

          {step === 2 && (
            <>
              {keyGroups().map((g) => {
                const specs = g.store.secrets.filter((x) => x.need !== "optional" && x.name !== g.store.notify_secret)
                return (
                  <div key={`${g.store.id}|${g.account}`} className="space-y-3">
                    <h3 className="text-sm font-medium">
                      {g.store.name}{g.account !== "default" && <span className="font-normal text-muted-foreground"> · account {g.account}</span>}
                    </h3>
                    {specs.map((spec) => (
                      <SecretInput key={spec.name} label={spec.label} file={spec.file}
                        value={keys[keyId(g.store.id, g.account, spec.name)] || ""}
                        onChange={(v) => setKeys((k) => ({ ...k, [keyId(g.store.id, g.account, spec.name)]: v }))}
                        isSet={g.builds.every((b) => has(spec.name, b))} />
                    ))}
                    <p className="text-xs text-muted-foreground">
                      Where to get {specs.length > 1 ? "these" : "it"}: docs/store-setup.md, {g.store.name}.
                      {g.store.notify_secret && " The secret of the notification URL is generated for you."}
                    </p>
                  </div>
                )
              })}
              {!keyGroups().length && <p className="text-sm text-muted-foreground">No store keys needed.</p>}
            </>
          )}

          {step === 3 && (
            <>
              <p className="text-sm text-muted-foreground">Store product ids. Prices are used for revenue when the store does not report one (Google Play, Amazon).</p>
              <div className="space-y-2">
                {w.products.map((p, i) => (
                  <div key={i} className="grid grid-cols-[1fr_10rem_6rem_5rem_auto] items-center gap-x-2 gap-y-1">
                    <Input className="font-mono text-xs" placeholder="product_id" value={p.product_id} onChange={(e) => updateProduct(i, { product_id: e.target.value })} />
                    <Select value={p.kind} onValueChange={(v) => updateProduct(i, { kind: v as Kind })}>
                      <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                      <SelectContent>{KINDS.map((k) => <SelectItem key={k} value={k}>{k}</SelectItem>)}</SelectContent>
                    </Select>
                    <Input type="number" step="0.01" min="0" placeholder="price" value={p.default_price_micros ? p.default_price_micros / 1e6 : ""}
                      onChange={(e) => updateProduct(i, { default_price_micros: Math.round(Number(e.target.value || 0) * 1e6) })} />
                    <Input placeholder="USD" value={p.currency} onChange={(e) => updateProduct(i, { currency: e.target.value.toUpperCase() })} />
                    <Button variant="ghost" size="icon" aria-label="Remove" onClick={() => setW((s) => ({ ...s, products: s.products.filter((_, j) => j !== i) }))}>
                      <Trash2Icon />
                    </Button>
                    {Object.keys(p.store_ids || {}).length > 0 && (
                      <p className="col-span-full truncate px-1 font-mono text-xs text-muted-foreground">
                        {Object.entries(p.store_ids!).map(([store, id]) => `${storeById(store)?.name || store}: ${id}`).join(" · ")}
                      </p>
                    )}
                  </div>
                ))}
                {!w.products.length && <p className="py-4 text-center text-sm text-muted-foreground">No products yet.</p>}
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <Button variant="outline" size="sm" onClick={() => set("products", [...w.products, { product_id: "", kind: "consumable", default_price_micros: 0, currency: "USD" }])}>
                  <PlusIcon /> Add product
                </Button>
                {importers.map((b) => (
                  <Button key={b.app_id} variant="outline" size="sm" onClick={() => loadCatalog(b)}><DownloadIcon /> Import from {storeOfBuild(b).name}</Button>
                ))}
                <span className="flex-1" />
                {apps().length > 1 && w.builds.map((b, i) => b.app_id && (
                  <div key={b.app_id} className="flex items-center gap-2">
                    <Checkbox id={`to-${i}`} checked={b.products} onCheckedChange={(v) => setBuild(i, { products: v === true })} />
                    <Label htmlFor={`to-${i}`} className="font-normal">{buildLabel(b)}</Label>
                  </div>
                ))}
              </div>
              {catalog && (
                <div className="space-y-3 rounded-lg border p-3">
                  {catalog.length ? <CatalogPicker rows={catalog} onChange={onCatalogPick} /> : <Loading rows={3} />}
                  <div className="flex justify-end"><Button size="sm" onClick={takeCatalog} disabled={!catalogPick.length}>Add selected</Button></div>
                </div>
              )}
            </>
          )}

          {step === 4 && (
            !reports ? (
              <div className="space-y-3"><p className="text-sm text-muted-foreground">Checking… (up to 10 seconds per app)</p><Loading rows={5} /></div>
            ) : (
              <div className="space-y-6">
                {reports.reports.map((r) => <ReportView key={r.app_id} report={r} />)}
                <Separator />
                <div className="space-y-3">
                  <h3 className="text-sm font-medium">Copy into the stores and the game</h3>
                  {reports.reports.map((r) => {
                    const u = reportUrls(r, reports.base, generated)
                    const b = apps().find((x) => x.app_id === r.app_id)
                    return u.notify && <CopyField key={r.app_id} label={`${b ? buildLabel(b) : r.app_id}: ${u.notifyLabel}`} value={u.notify} />
                  })}
                  <CopyField label="Webhook HMAC key (shown once)" value={generated.webhook_secret || ""} />
                  <CopyField label="Unity config: paste it into the Purser config inspector" value={unityConfig} />
                  {Object.keys(generated).some((k) => k !== "webhook_secret") && <p className="text-xs text-muted-foreground">Notification URLs with a new secret are shown only now; the secret is stored on the server.</p>}
                </div>
              </div>
            )
          )}
        </div>
      </DialogBody>
      <DialogFooter className="gap-2">
        {step > 0 && step < 4 && <Button variant="outline" onClick={() => setStep((s) => s - 1)} disabled={busy}>Back</Button>}
        {step < 4 ? (
          <Button onClick={next} disabled={busy}>{step === 3 ? "Save and check" : "Save and continue"}</Button>
        ) : (
          <>
            {hasAppleBuild && (
              <Button variant="outline" onClick={() => runChecks(w, true)} disabled={!reports}
                title="Asks Apple to send a TEST notification to this server; the next check shows whether it arrived">
                Apple test notification
              </Button>
            )}
            <Button variant="outline" onClick={() => runChecks()} disabled={!reports}>Check again</Button>
            <Button onClick={() => onClose(w.game_key)}>Done</Button>
          </>
        )}
      </DialogFooter>
    </>
  )
}

/** "Paste from Unity": the config the Unity inspector copies (builds and products). */
function UnityPaste({ onApply }: { onApply: (c: ParsedUnityConfig) => void }) {
  const [open, setOpen] = useState(false)
  const [text, setText] = useState("")
  const [error, setError] = useState<string | null>(null)
  const apply = () => {
    try {
      onApply(parseUnityConfig(text))
      setOpen(false)
      setText("")
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => { setOpen(o); if (!o) setError(null) }}>
      <Button variant="outline" size="sm" onClick={() => setOpen(true)}><ClipboardPasteIcon /> Paste from Unity</Button>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Paste from Unity</DialogTitle>
          <DialogDescription>
            The JSON the Purser config inspector copies. It fills in the builds (on the default developer account) and the products;
            prices can be set in the Products step.
          </DialogDescription>
        </DialogHeader>
        <Textarea className="min-h-56 font-mono text-xs" value={text} onChange={(e) => { setText(e.target.value); setError(null) }} spellCheck={false}
          placeholder={'{"serverBaseUrl": "...", "builds": [{"store": "google", "appId": "com.example.mygame"}], "products": [...]}'} />
        {error && <ErrorBox error={error} />}
        <DialogFooter><Button onClick={apply} disabled={!text.trim()}>Fill in</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
