import { useMemo, useState } from "react"
import {
  BellRingIcon, ChartLineIcon, Gamepad2Icon, PencilIcon, GlobeIcon, KeyRoundIcon, LayoutTemplateIcon, ListChecksIcon, PackageIcon, PlugIcon, ReceiptIcon,
  RefreshCwIcon, SendIcon, ShoppingBagIcon, SmartphoneIcon, SparklesIcon, TabletSmartphoneIcon, Trash2Icon, WebhookIcon, type LucideIcon,
} from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { AccountSelect, accountsOf } from "@/components/account-select"
import { AddGameWizard } from "@/components/add-game-wizard"
import { CopyField, DataTable, ErrorBox, Field, LevelBadge, Loading, Mono, Section, Tone } from "@/components/common"
import { ReportView, reportUrls } from "@/components/report"
import { SecretDialog, type SecretTarget } from "@/components/secret-dialog"
import { call, genSecret, proc, publicRoute, sql, sqlStr } from "@/lib/api"
import { appsChanged, useApps, type AppGroup } from "@/lib/apps"
import { checkBuilds } from "@/lib/checks"
import { buildLabel, platformById, storeOf, STORE_SECRETS } from "@/lib/stores"
import { tryAct, useAsync } from "@/lib/hooks"
import type { GameApp, IntegrationHealth, Level, Report } from "@/lib/types"
import { cn } from "@/lib/utils"
import { AppIntegrationsTab } from "@/pages/app-integrations"
import { AppPaywallsTab } from "@/pages/app-paywalls"
import { AppProductsTab } from "@/pages/app-products"
import { PurchasesPage } from "@/pages/purchases"
import { RevenuePage } from "@/pages/revenue"
import { APP_TABS, appRoute, type AppTab, type PageProps } from "@/routes"

const TAB_TITLES: Record<AppTab, string> = {
  setup: "Setup", products: "Products", paywalls: "Paywalls", delivery: "Delivery", integrations: "Integrations", keys: "Keys", purchases: "Purchases", revenue: "Revenue",
}

const TAB_ICONS: Record<AppTab, LucideIcon> = {
  setup: ListChecksIcon, products: PackageIcon, paywalls: LayoutTemplateIcon, delivery: SendIcon, integrations: PlugIcon, keys: KeyRoundIcon, purchases: ReceiptIcon, revenue: ChartLineIcon,
}

export interface AppData {
  group: AppGroup
  /** Developer accounts named by any app (not only this one), without "default". */
  accounts: string[]
  creds: Set<string>
  integrations: { app_id: string; kind: string; enabled: boolean; settings_json: string; key: string }[]
  /** What happened to the events of each integration (`integration_health`). */
  health: IntegrationHealth[]
  reports: Record<string, Report | null>
}

export interface TabProps {
  data: AppData
  reload: () => void
  go: (route: string) => void
}

/** Per-build rows of the app's builds, read by key (`game_app` comes from the shared `useApps`). */
async function loadApp(ids: string[]): Promise<Omit<AppData, "group" | "accounts">> {
  const each = async <T,>(query: (id: string) => string) => (await Promise.all(ids.map((id) => sql<T>(query(id))))).flat()
  const [creds, integrations, health, checks] = await Promise.all([
    sql<{ key: string }>("SELECT key FROM credential"),
    each<AppData["integrations"][number]>((id) => `SELECT * FROM integration WHERE app_id = ${sqlStr(id)}`),
    each<IntegrationHealth>((id) => `SELECT * FROM integration_health WHERE app_id = ${sqlStr(id)}`),
    each<{ app_id: string; report_json: string }>((id) => `SELECT app_id, report_json FROM app_check WHERE app_id = ${sqlStr(id)}`),
  ])
  const reports: Record<string, Report | null> = {}
  for (const h of checks) {
    try { reports[h.app_id] = JSON.parse(h.report_json) } catch { reports[h.app_id] = null }
  }
  return { creds: new Set(creds.map((c) => c.key)), integrations, health, reports }
}

export const PLATFORM_ICON: Record<string, LucideIcon> = {
  android: SmartphoneIcon,
  ios: TabletSmartphoneIcon,
  amazon: ShoppingBagIcon,
  steam: Gamepad2Icon,
  web: GlobeIcon,
}

const LEVEL_DOT: Partial<Record<Level, [string, string]>> = {
  ok: ["bg-emerald-500", "Setup check passed"],
  warn: ["bg-amber-500", "Setup check has warnings"],
  fail: ["bg-red-500", "Setup check failed"],
}

/** A build in the app header: platform, store, bundle id (click to copy) and the setup check result. */
function BuildChip({ app, level }: { app: GameApp; level?: Level }) {
  const Icon = PLATFORM_ICON[app.platform] || SmartphoneIcon
  const [dot, hint] = (level && LEVEL_DOT[level]) || ["bg-muted-foreground/40", "Not checked yet"]
  const copy = () => navigator.clipboard?.writeText(app.app_id).then(() => toast.success(`Copied ${app.app_id}`))
  return (
    <button
      type="button"
      onClick={copy}
      title={`${hint}. Click to copy the app id.`}
      className={cn(
        "group inline-flex max-w-full items-center gap-2 rounded-lg border bg-card py-1 ps-1 pe-2.5 text-start shadow-xs transition-colors hover:bg-muted",
        !app.enabled && "opacity-60",
      )}
    >
      <span className="flex size-7 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground group-hover:bg-background">
        <Icon className="size-3.5" />
      </span>
      <span className="flex min-w-0 flex-col leading-tight">
        <span className="flex items-center gap-1.5 text-xs font-medium">
          {platformById(app.platform)?.name || app.platform}
          {buildLabel(app).includes("·") && <span className="font-normal text-muted-foreground">{storeOf(app).name}</span>}
          {!app.enabled && <span className="font-normal text-muted-foreground">· disabled</span>}
        </span>
        <span className="truncate font-mono text-[11px] text-muted-foreground">{app.app_id}</span>
      </span>
      <span className={cn("ms-1 size-2 shrink-0 rounded-full", dot)} />
    </button>
  )
}

/** Edits the app's display name (`app_profile`); the key stays as it is. */
function RenameButton({ group }: { group: AppGroup }) {
  const [open, setOpen] = useState(false)
  const [name, setName] = useState("")
  const [busy, setBusy] = useState(false)
  const start = () => {
    setName(group.name === group.key ? "" : group.name)
    setOpen(true)
  }
  const save = async () => {
    setBusy(true)
    const done = await tryAct(async () => {
      await call("admin_set_app_name", group.key, name.trim())
      return true
    }, name.trim() ? "Renamed" : "Name removed")
    setBusy(false)
    if (done) {
      setOpen(false)
      appsChanged()
    }
  }
  return (
    <>
      <Button variant="ghost" size="icon" className="size-7 shrink-0 text-muted-foreground" title="Rename" aria-label="Rename the app" onClick={start}>
        <PencilIcon className="size-3.5" />
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Rename the app</DialogTitle>
            <DialogDescription>
              The name shown in the admin panel. The app key <Mono>{group.key}</Mono> stays the same: paywalls, revenue and
              shared entitlements are keyed by it.
            </DialogDescription>
          </DialogHeader>
          <Field label="Display name" hint="Leave empty to show the app key.">
            <Input value={name} maxLength={80} placeholder={group.key} autoFocus
              onChange={(e) => setName(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && !busy) save() }} />
          </Field>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button onClick={save} disabled={busy}>Save</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

export function AppPage({ appKey, tab, go, me }: PageProps & { appKey: string; tab: AppTab }) {
  const { apps, groups, error: appsError } = useApps()
  // Apps without a game_key are listed under their app id.
  const group = groups?.find((g) => g.key === appKey)
  const idKey = group ? group.apps.map((a) => a.app_id).join(",") : ""
  const loaded = useAsync(async () => (idKey ? loadApp(idKey.split(",")) : null), [idKey])
  const accounts = useMemo(() => accountsOf(apps || []), [apps])
  if (appsError || loaded.error) return <ErrorBox error={appsError || loaded.error} />
  if (!groups) return <Loading rows={6} />
  if (!group) return <ErrorBox error={`No app "${appKey}"`} hint={<Button variant="link" className="px-0" onClick={() => go("apps")}>Back to apps</Button>} />
  if (!loaded.data) return <Loading rows={6} />
  const data: AppData = { ...loaded.data, group, accounts }
  const reload = loaded.reload
  const g = data.group
  const ids = g.apps.map((a) => a.app_id)
  const props: TabProps = { data, reload, go }
  return (
    <>
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <div className="flex min-w-0 items-center gap-1">
            <h1 className="truncate text-xl font-semibold tracking-tight">{g.name}</h1>
            <RenameButton group={g} />
            {g.name !== g.key && <span className="ms-1 truncate font-mono text-xs text-muted-foreground" title="App key">{g.key}</span>}
          </div>
          <div className="flex flex-wrap gap-2 pt-1">
            {g.apps.map((a) => <BuildChip key={a.app_id} app={a} level={data.reports[a.app_id]?.summary} />)}
          </div>
        </div>
      </div>
      <Tabs value={tab} onValueChange={(t) => go(appRoute(g.key, t as AppTab))}>
        <TabsList variant="line" className="h-auto w-full flex-wrap justify-start gap-x-1 border-b pb-1">
          {APP_TABS.map((t) => {
            const Icon = TAB_ICONS[t]
            return <TabsTrigger key={t} value={t} className="flex-none px-2"><Icon /> {TAB_TITLES[t]}</TabsTrigger>
          })}
        </TabsList>
      </Tabs>
      {tab === "setup" && <SetupTab {...props} />}
      {tab === "products" && <AppProductsTab {...props} />}
      {tab === "paywalls" && <AppPaywallsTab {...props} />}
      {tab === "delivery" && <DeliveryTab {...props} />}
      {tab === "integrations" && <AppIntegrationsTab {...props} />}
      {tab === "keys" && <KeysTab {...props} />}
      {tab === "purchases" && <PurchasesPage go={go} me={me} appIds={ids} embedded />}
      {tab === "revenue" && <RevenuePage go={go} me={me} fixedGame={g.key} embedded />}
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Setup: the setup check of every build and the URLs to paste into the stores
// ---------------------------------------------------------------------------------------------

function SetupTab({ data, reload }: TabProps) {
  const g = data.group
  const [checking, setChecking] = useState<"" | "all" | "apple">("")
  const [wizard, setWizard] = useState(false)
  const [secrets, setSecrets] = useState<Record<string, string>>({})
  const base = useAsync(() => publicRoute())
  const appleBuilds = g.apps.filter((a) => storeOf(a).id === "apple")
  /** Checks the builds at once; `appleTest` re-checks the App Store builds with a test notification. */
  const check = async (appleTest: boolean) => {
    setChecking(appleTest ? "apple" : "all")
    const reports = await checkBuilds((appleTest ? appleBuilds : g.apps).map((a) => a.app_id), appleTest)
    const failed = reports.find((r) => r.checks.length === 1 && r.checks[0].id === "error")
    if (failed) toast.error(failed.checks[0].detail)
    setChecking("")
    reload()
  }
  const newRtdn = async (appId: string) => {
    if (!confirm("Replace the RTDN secret? Notifications fail until the Pub/Sub push URL is updated.")) return
    const secret = genSecret()
    const ok = await tryAct(() => call("admin_set_credential", appId, "google_rtdn_secret", secret), "New secret saved: copy the URL now")
    if (ok !== undefined) { setSecrets({ google_rtdn_secret: secret }); await tryAct(() => proc("admin_check_app", appId, JSON.stringify({ online: false, apple_test: false }))); reload() }
  }
  return (
    <>
      <div className="flex flex-wrap items-center gap-2">
        <Button onClick={() => check(false)} disabled={!!checking}><RefreshCwIcon className={checking === "all" ? "animate-spin" : ""} /> {checking === "all" ? "Checking…" : "Check now"}</Button>
        {appleBuilds.length > 0 && (
          <Button variant="outline" onClick={() => check(true)} disabled={!!checking}
            title="Asks Apple to send a TEST notification to this server; the next check shows whether it arrived">
            <BellRingIcon className={checking === "apple" ? "animate-pulse" : ""} /> {checking === "apple" ? "Asking Apple…" : "Apple test notification"}
          </Button>
        )}
        <Button variant="outline" onClick={() => setWizard(true)}><SparklesIcon /> Add a build or run the setup wizard</Button>
      </div>
      {g.apps.map((a) => {
        const rep = data.reports[a.app_id]
        const urls = rep && base.data ? reportUrls(rep, base.data, secrets) : null
        return (
          <Section key={a.app_id} title={<span className="flex items-center gap-2">{buildLabel(a)} <Mono className="font-normal">{a.app_id}</Mono></span>}
            actions={rep ? <LevelBadge level={rep.summary} /> : <Tone tone="muted">not checked</Tone>}>
            {!rep ? (
              <p className="text-sm text-muted-foreground">Not checked yet: press Check now.</p>
            ) : (
              <div className="space-y-5">
                <ReportView report={rep} />
                {urls && (
                  <div className="space-y-3">
                    {urls.notify && <CopyField label={urls.notifyLabel} value={urls.notify} />}
                    {urls.rtdn.includes("<google_rtdn_secret>") && (
                      <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                        The stored secret is never shown. Generate a new one to get the full URL, then update the Pub/Sub push subscription.
                        <Button size="xs" variant="outline" onClick={() => newRtdn(a.app_id)}><KeyRoundIcon /> New RTDN secret</Button>
                      </div>
                    )}
                    <CopyField label="Billing identity (the app database must trust it)" value={urls.identity} />
                  </div>
                )}
              </div>
            )}
          </Section>
        )
      })}
      <AddGameWizard open={wizard} existing={g} onClose={(_, changed) => { setWizard(false); if (changed) { appsChanged(); reload() } }} />
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Delivery: how rewards reach the app, shared by its builds
// ---------------------------------------------------------------------------------------------

type Common = Pick<GameApp, "delivery" | "spacetime_url" | "spacetime_db" | "grant_reducer" | "revoke_reducer" | "args_format" | "webhook_url">
type Flags = Pick<GameApp, "enabled" | "allow_sandbox" | "client_consumes" | "account" | "apple_refund_mode">

const REFUND_MODES = [
  { value: "off", label: "Don't answer" },
  { value: "inform", label: "Send consumption data" },
  { value: "prefer", label: "Send data and recommend an outcome" },
]

function DeliveryTab({ data, reload, go }: TabProps) {
  const g = data.group
  const first = g.apps[0]
  const [common, setCommon] = useState<Common>({
    delivery: first.delivery, spacetime_url: first.spacetime_url || "https://maincloud.spacetimedb.com", spacetime_db: first.spacetime_db,
    grant_reducer: first.grant_reducer || "billing_grant", revoke_reducer: first.revoke_reducer || "billing_revoke", args_format: first.args_format || "v1", webhook_url: first.webhook_url,
  })
  const [flags, setFlags] = useState<Record<string, Flags>>(() =>
    Object.fromEntries(g.apps.map((a) => [a.app_id, { enabled: a.enabled, allow_sandbox: a.allow_sandbox, client_consumes: a.client_consumes, account: a.account || "default", apple_refund_mode: a.apple_refund_mode || "off" }])))
  const set = <K extends keyof Common>(k: K, v: Common[K]) => setCommon((c) => ({ ...c, [k]: v }))
  const differs = g.apps.some((a) => a.delivery !== first.delivery || a.spacetime_db !== first.spacetime_db || a.webhook_url !== first.webhook_url)

  const save = async () => {
    const ok = await tryAct(async () => {
      for (const a of g.apps) {
        await call("admin_upsert_game_app", JSON.stringify({
          app_id: a.app_id, platform: a.platform, store: a.store, game_key: g.key, ...common,
          spacetime_url: common.delivery === "spacetimedb" ? common.spacetime_url : "", ...flags[a.app_id],
        }))
      }
      return true
    }, "Saved")
    if (ok) { appsChanged(); reload() }
  }
  const remove = async (a: GameApp) => {
    if (!confirm(`Delete the ${buildLabel(a)} build ${a.app_id}? Its products and purchase ledger are kept.`)) return
    const ok = await tryAct(() => call("admin_delete_game_app", a.app_id), "Deleted")
    if (ok === undefined) return
    appsChanged()
    if (g.apps.length === 1) go("apps")
    else reload()
  }
  const ping = async (appId: string) => {
    const r = await tryAct(() => proc<{ ok: boolean; status: number; body: string }>("admin_ping_webhook", appId))
    if (r) (r.ok ? toast.success : toast.error)(`Webhook answered HTTP ${r.status}${r.ok ? "" : ": " + r.body}`)
  }

  return (
    <>
      <Section title="Reward delivery" description="How purchases reach the app. Shared by all its builds."
        actions={<Button onClick={save}>Save</Button>}>
        {differs && <p className="mb-4 text-sm text-amber-700 dark:text-amber-400">The builds have different settings now; saving applies these to all of them.</p>}
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Delivery">
            <Select value={common.delivery} onValueChange={(v) => set("delivery", v as GameApp["delivery"])}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="spacetimedb">Reducers in the app's SpacetimeDB database</SelectItem>
                <SelectItem value="webhook">Webhook to the app backend</SelectItem>
                <SelectItem value="none">No backend: the client grants rewards</SelectItem>
              </SelectContent>
            </Select>
          </Field>
          {common.delivery === "spacetimedb" && (
            <>
              <Field label="SpacetimeDB server (https, allowed host)"><Input value={common.spacetime_url} onChange={(e) => set("spacetime_url", e.target.value)} /></Field>
              <Field label="Database name"><Input value={common.spacetime_db} onChange={(e) => set("spacetime_db", e.target.value)} /></Field>
              <Field label="Grant reducer"><Input value={common.grant_reducer} onChange={(e) => set("grant_reducer", e.target.value)} /></Field>
              <Field label="Revoke reducer"><Input value={common.revoke_reducer} onChange={(e) => set("revoke_reducer", e.target.value)} /></Field>
              <Field label="Arguments format" hint="legacy: only for apps migrated from Adapty">
                <Select value={common.args_format} onValueChange={(v) => set("args_format", v)}>
                  <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                  <SelectContent><SelectItem value="v1">v1</SelectItem><SelectItem value="legacy">legacy</SelectItem></SelectContent>
                </Select>
              </Field>
            </>
          )}
          {common.delivery === "webhook" && (
            <Field label="Webhook URL (https)" className="sm:col-span-2"><Input value={common.webhook_url} onChange={(e) => set("webhook_url", e.target.value)} /></Field>
          )}
        </div>
      </Section>
      <Section title="Builds" description="Each build reads its store keys from its developer account: a game can be published on Google Play, the App Store and the Amazon Appstore from different accounts.">
        <DataTable
          rows={g.apps}
          columns={[
            { title: "Platform", cell: (a) => buildLabel(a) },
            { title: "Store", cell: (a) => storeOf(a).name },
            { title: "App id", cell: (a) => <Mono>{a.app_id}</Mono> },
            {
              title: "Developer account",
              cell: (a) => (
                <AccountSelect className="w-48" value={flags[a.app_id]?.account || "default"} accounts={data.accounts}
                  onChange={(account) => setFlags((f) => ({ ...f, [a.app_id]: { ...f[a.app_id], account } }))} />
              ),
            },
            ...(["enabled", "allow_sandbox", "client_consumes"] as const).map((k) => ({
              title: { enabled: "Enabled", allow_sandbox: "Sandbox purchases", client_consumes: "Client consumes" }[k],
              cell: (a: GameApp) => (
                <Checkbox checked={!!flags[a.app_id]?.[k]} disabled={k === "client_consumes" && storeOf(a).finish !== "server_consume_acknowledge"}
                  onCheckedChange={(v) => setFlags((f) => ({ ...f, [a.app_id]: { ...f[a.app_id], [k]: v === true } }))} />
              ),
            })),
            ...(g.apps.some((a) => storeOf(a).id === "apple") ? [{
              title: "Refund requests",
              cell: (a: GameApp) => storeOf(a).id !== "apple" ? <span className="text-muted-foreground">-</span> : (
                <Select value={flags[a.app_id]?.apple_refund_mode || "off"}
                  onValueChange={(v) => setFlags((f) => ({ ...f, [a.app_id]: { ...f[a.app_id], apple_refund_mode: v } }))}>
                  <SelectTrigger size="sm" className="w-56"><SelectValue /></SelectTrigger>
                  <SelectContent>{REFUND_MODES.map((m) => <SelectItem key={m.value} value={m.value}>{m.label}</SelectItem>)}</SelectContent>
                </Select>
              ),
            }] : []),
            {
              title: "",
              className: "text-right",
              cell: (a) => (
                <span className="inline-flex gap-1">
                  {first.delivery === "webhook" && <Button size="xs" variant="outline" onClick={() => ping(a.app_id)}><WebhookIcon /> Ping</Button>}
                  <Button size="icon-sm" variant="ghost" className="text-destructive" aria-label="Delete build" onClick={() => remove(a)}><Trash2Icon /></Button>
                </span>
              ),
            },
          ]}
        />
        <p className="mt-3 text-xs text-muted-foreground">
          Client consumes: the Android client consumes purchases itself (e.g. still through Adapty); otherwise consumed purchases unknown to the ledger are rejected as granted elsewhere.
          Refund requests: when an App Store player asks for a refund, Apple asks the server whether the purchase was delivered and used; the answer is sent only for players who consented in the game (<Mono>SetRefundDataConsent</Mono> in the Unity package).
          Press Save above to apply.
        </p>
      </Section>
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Keys: every secret the app needs, and where its value comes from
// ---------------------------------------------------------------------------------------------

/** Secrets usually shared by a developer account; the rest belong to one build. */
const ACCOUNT_SECRETS = new Set([
  ...STORE_SECRETS.filter((s) => s.account_wide).map((s) => s.name), "appsflyer_s2s_key", "tenjin_sdk_key", "spacetime_token", "webhook_secret",
])
const INTEGRATION_SECRET: Record<string, string> = {
  ga4: "ga4_api_secret", appsflyer: "appsflyer_s2s_key", tenjin: "tenjin_sdk_key", webhook: "integration_webhook_secret",
  adjust: "adjust_s2s_token", appmetrica: "appmetrica_post_api_key", gcs: "gcs_service_account_json",
}

/** (secret, required) pairs a build needs with its current store, delivery and integrations. */
function neededSecrets(a: GameApp, kinds: string[]): [string, boolean][] {
  const out: [string, boolean][] = storeOf(a).secrets.map((s) => [s.name, s.need !== "optional"])
  if (a.delivery === "spacetimedb") out.push(["spacetime_token", true])
  if (a.delivery === "webhook") out.push(["webhook_secret", true])
  for (const k of kinds) if (INTEGRATION_SECRET[k]) out.push([INTEGRATION_SECRET[k], true])
  return out
}

function KeysTab({ data, reload }: TabProps) {
  const g = data.group
  const allApps = useApps().apps
  const [target, setTarget] = useState<SecretTarget | null>(null)
  const resolve = (name: string, a: GameApp) => {
    for (const [scope, label] of [[a.app_id, "this build"], [a.account, `account ${a.account}`], ["default", "default"]] as const) {
      if (data.creds.has(`${scope}/${name}`)) return label
    }
    return null
  }
  const names = new Map<string, boolean>()
  for (const a of g.apps) {
    const kinds = data.integrations.filter((i) => i.app_id === a.app_id && i.enabled).map((i) => i.kind)
    for (const [n, req] of neededSecrets(a, kinds)) names.set(n, (names.get(n) || false) || req)
  }
  const rows = [...names.entries()].map(([name, required]) => ({ name, required }))
  const accounts = [...new Set(g.apps.map((a) => a.account || "default"))]
  /** Several developer accounts exist: a store key in `default` would be used by the wrong account's apps. */
  const sharedDefault = data.accounts.length > 0
  const open = (name: string) => {
    const apps = g.apps.filter((a) => neededSecrets(a, data.integrations.filter((i) => i.app_id === a.app_id && i.enabled).map((i) => i.kind)).some(([n]) => n === name))
    const appScopes = apps.map((a) => ({ value: a.app_id, label: `Only the ${buildLabel(a)} build (${a.app_id})` }))
    const accountScopes = [...new Set(apps.map((a) => a.account || "default"))].filter((acc) => acc !== "default").map((acc) => ({
      value: acc,
      label: `Account ${acc}: every app of this developer account (${apps.filter((a) => a.account === acc).map((a) => buildLabel(a)).join(", ")})`,
    }))
    // The shared default is offered only for builds without a developer account (their account scope is
    // `default`); from a build with its own account it would silently change the key of other apps.
    // The default itself is set on the Server page.
    const noAccount = apps.some((a) => !a.account || a.account === "default")
    const shared = (allApps || []).filter((a) => !a.account || a.account === "default").length
    const defaultScope = noAccount
      ? [{ value: "default", label: `Default: shared by every app without a developer account (${shared} ${shared === 1 ? "build" : "builds"})` }]
      : []
    setTarget({ name, scopes: ACCOUNT_SECRETS.has(name) ? [...accountScopes, ...appScopes, ...defaultScope] : [...appScopes, ...accountScopes, ...defaultScope] })
  }
  return (
    <>
      <Section title="Keys" description={<>What each build needs with its current store, delivery and integrations, and where the value comes from: the build itself, its developer account ({accounts.map((a, i) => <span key={a}>{i > 0 && ", "}<code>{a}</code></span>)}) or the default. Values are never shown.</>}>
        <DataTable
          rows={rows}
          columns={[
            { title: "Key", cell: (r) => <span className="flex items-center gap-2"><Mono>{r.name}</Mono>{!r.required && <Tone tone="muted">optional</Tone>}</span> },
            ...g.apps.map((a) => ({
              title: buildLabel(a),
              cell: (r: { name: string; required: boolean }) => {
                const needed = neededSecrets(a, data.integrations.filter((i) => i.app_id === a.app_id && i.enabled).map((i) => i.kind)).some(([n]) => n === r.name)
                if (!needed) return <span className="text-muted-foreground">—</span>
                const from = resolve(r.name, a)
                if (from === "default" && sharedDefault && ACCOUNT_SECRETS.has(r.name)) {
                  return <span title={`This key is in default, shared by every account without its own key. Set it for account ${a.account} if this build is published from its own account.`}><Tone tone="warn">default</Tone></span>
                }
                return from ? <Tone tone="ok">{from}</Tone> : r.required ? <Tone tone="bad">missing</Tone> : <Tone tone="muted">not set</Tone>
              },
            })),
            { title: "", className: "text-right", cell: (r) => <Button size="xs" variant="outline" onClick={() => open(r.name)}>Set</Button> },
          ]}
        />
      </Section>
      <SecretDialog target={target} onClose={() => setTarget(null)} onSaved={reload} />
    </>
  )
}

