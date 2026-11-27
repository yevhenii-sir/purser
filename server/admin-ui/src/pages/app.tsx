import { useMemo, useState } from "react"
import { BellRingIcon, ChartLineIcon, KeyRoundIcon, LayoutTemplateIcon, ListChecksIcon, PackageIcon, PencilIcon, PlugIcon, ReceiptIcon, RefreshCwIcon, SendIcon, SmartphoneIcon, SparklesIcon, TicketIcon, Trash2Icon, WebhookIcon, type LucideIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { AccountSelect } from "@/components/account-select"
import { AddGameWizard } from "@/components/add-game-wizard"
import { CopyField, DataTable, ErrorBox, Field, LevelBadge, Loading, Mono, Section, Tone } from "@/components/common"
import { DeliveryFields } from "@/components/delivery-fields"
import { KINDS } from "@/components/integration-kinds"
import { ReportView } from "@/components/report"
import { SecretDialog, type SecretTarget } from "@/components/secret-dialog"
import { StoreSetupButton } from "@/components/store-setup"
import { call, proc, publicRoute, select } from "@/lib/api"
import { accountsOf, appsChanged, useApps, type AppGroup } from "@/lib/apps"
import { checkBuilds, reportUrls } from "@/lib/checks"
import { DELIVERY_DEFAULTS, type DeliverySettings } from "@/lib/delivery"
import { buildLabel, PLATFORM_ICON, platformById, storeOf, STORE_SECRETS } from "@/lib/stores"
import { tryAct, useAsync, useCopy } from "@/lib/hooks"
import { fmtTime } from "@/lib/format"
import type { GameApp, IntegrationHealth, Level, Report } from "@/lib/types"
import { cn, parseJsonOr } from "@/lib/utils"
import { AppIntegrationsTab } from "@/pages/app-integrations"
import { AppPaywallsTab } from "@/pages/app-paywalls"
import { AppPromoTab } from "@/pages/app-promo"
import { AppProductsTab } from "@/pages/app-products"
import { PurchasesPage } from "@/pages/purchases"
import { RevenuePage } from "@/pages/revenue"
import { APP_TABS, appRoute, type AppTab, type PageProps } from "@/routes"
import { useAllowed, useCan } from "@/lib/roles"

const TAB_TITLES: Record<AppTab, string> = {
  setup: "Setup", products: "Products", paywalls: "Paywalls", promo: "Promo codes", delivery: "Delivery", integrations: "Integrations", keys: "Keys", purchases: "Purchases", revenue: "Revenue",
}

const TAB_ICONS: Record<AppTab, LucideIcon> = {
  setup: ListChecksIcon, products: PackageIcon, paywalls: LayoutTemplateIcon, promo: TicketIcon, delivery: SendIcon, integrations: PlugIcon, keys: KeyRoundIcon, purchases: ReceiptIcon, revenue: ChartLineIcon,
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
  const byApp = { column: "app_id", equals: ids }
  const [creds, integrations, health, checks] = await Promise.all([
    select<{ key: string }>({ table: "credential", columns: ["key"] }),
    select<AppData["integrations"][number]>({ table: "integration", where: byApp }),
    select<IntegrationHealth>({ table: "integration_health", where: byApp }),
    select<{ app_id: string; report_json: string }>({ table: "app_check", columns: ["app_id", "report_json"], where: byApp }),
  ])
  const reports: Record<string, Report | null> = {}
  for (const h of checks) reports[h.app_id] = parseJsonOr<Report | null>(h.report_json, null)
  return { creds: new Set(creds.map((c) => c.key)), integrations, health, reports }
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
  const { copy } = useCopy()
  return (
    <button
      type="button"
      onClick={() => copy(app.app_id, `Copied ${app.app_id}`)}
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
          {buildLabel(app)}
          {!app.enabled && <span className="font-normal text-muted-foreground">· disabled</span>}
        </span>
        <span className="truncate font-mono text-[11px] text-muted-foreground">{app.store_app_id}</span>
      </span>
      <span className={cn("ms-1 size-2 shrink-0 rounded-full", dot)} />
    </button>
  )
}

/** Edits the app's display name (`app_profile`, the key stays as it is) and its privacy policy
 * (`app_policy`: no advertising ids for child-directed apps). */
function RenameButton({ group }: { group: AppGroup }) {
  const [open, setOpen] = useState(false)
  const [name, setName] = useState("")
  const [noAdIds, setNoAdIds] = useState(false)
  const [storedNoAdIds, setStoredNoAdIds] = useState(false)
  const [busy, setBusy] = useState(false)
  const start = () => {
    setName(group.name === group.key ? "" : group.name)
    setOpen(true)
    select<{ game_key: string; no_ad_ids: boolean }>({ table: "app_policy", where: { column: "game_key", equals: group.key } })
      .then((rows) => {
        const v = rows.some((r) => r.no_ad_ids)
        setNoAdIds(v)
        setStoredNoAdIds(v)
      })
      .catch(() => {})
  }
  const save = async () => {
    setBusy(true)
    const done = await tryAct(async () => {
      await call("admin_set_app_name", group.key, name.trim())
      if (noAdIds !== storedNoAdIds) await call("admin_set_app_policy", group.key, noAdIds)
      return true
    }, "Saved")
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
            <DialogTitle>App settings</DialogTitle>
            <DialogDescription>
              The name shown in the admin panel. The app key <Mono>{group.key}</Mono> stays the same: paywalls, revenue and
              shared entitlements are keyed by it.
            </DialogDescription>
          </DialogHeader>
          <Field label="Display name" hint="Leave empty to show the app key.">
            <Input value={name} maxLength={80} placeholder={group.key} autoFocus
              onChange={(e) => setName(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter" && !busy) save() }} />
          </Field>
          <div className="flex items-start gap-2">
            <Checkbox id="no-ad-ids" checked={noAdIds} onCheckedChange={(v) => setNoAdIds(v === true)} className="mt-0.5" />
            <div className="min-w-0">
              <label htmlFor="no-ad-ids" className="text-sm">Child-directed app: keep no advertising ids</label>
              <p className="text-xs text-muted-foreground">
                Google Play Families, Apple Kids Category, COPPA: advertising ids the client sends (GAID, IDFA, OAID, Apple Ads,
                Meta) are dropped, so they are neither stored nor sent to integrations.
              </p>
            </div>
          </div>
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
  const renames = useAllowed("configure")
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
            {renames && <RenameButton group={g} />}
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
      {tab === "promo" && <AppPromoTab {...props} />}
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
  /** What is being checked: every build (`all`), or the App Store build of this key asking Apple for a test notification. */
  const [checking, setChecking] = useState("")
  const [wizard, setWizard] = useState(false)
  const base = useAsync(() => publicRoute())
  const can = useCan()
  // Store setup writes keys (the server checks Secrets for every store); checks and the wizard need Configure.
  const canSetUp = (_: GameApp) => can("secrets")
  const configure = can("configure")
  const recheck = async (appId: string) => {
    await checkBuilds([appId])
    reload()
  }
  /** Checks every build at once, or (`apple`) one App Store build with a test notification. */
  const check = async (apple?: GameApp) => {
    setChecking(apple ? apple.app_id : "all")
    const reports = await checkBuilds(apple ? [apple.app_id] : g.apps.map((a) => a.app_id), !!apple)
    const failed = reports.find((r) => r.checks.length === 1 && r.checks[0].id === "error")
    if (failed) toast.error(failed.checks[0].detail)
    setChecking("")
    reload()
  }
  return (
    <>
      {configure && <div className="flex flex-wrap items-center gap-2">
        <Button onClick={() => check()} disabled={!!checking}><RefreshCwIcon className={checking === "all" ? "animate-spin" : ""} /> {checking === "all" ? "Checking…" : "Check now"}</Button>
        <Button variant="outline" onClick={() => setWizard(true)}><SparklesIcon /> Add a build or run the setup wizard</Button>
      </div>}
      {g.apps.map((a) => {
        const rep = data.reports[a.app_id]
        const urls = rep && base.data ? reportUrls(rep, base.data) : null
        return (
          <Section key={a.app_id} title={<span className="flex items-center gap-2">{buildLabel(a)} <Mono className="font-normal">{a.store_app_id}</Mono></span>}
            actions={
              <div className="flex flex-wrap items-center justify-end gap-2">
                {configure && storeOf(a).id === "apple" && (
                  <Button variant="outline" size="xs" onClick={() => check(a)} disabled={!!checking}
                    title="Asks Apple to send a TEST notification to this server and checks the key with Apple; the next check shows whether it arrived">
                    <BellRingIcon className={checking === a.app_id ? "animate-pulse" : ""} /> {checking === a.app_id ? "Asking Apple…" : "Test notification"}
                  </Button>
                )}
                {canSetUp(a) && <StoreSetupButton app={a} base={base.data ?? ""} onDone={() => recheck(a.app_id)} />}
                {rep ? <LevelBadge level={rep.summary} /> : <Tone tone="muted">not checked</Tone>}
              </div>
            }>
            {!rep ? (
              <p className="text-sm text-muted-foreground">{configure ? "Not checked yet: press Check now." : "Not checked yet."}</p>
            ) : (
              <div className="space-y-5">
                <ReportView report={rep} />
                {urls && (
                  <div className="space-y-3">
                    {urls.notify && <CopyField label={urls.notifyLabel} value={urls.notify} />}
                    {urls.pushAudience && (
                      <>
                        <p className="text-xs text-muted-foreground">
                          Turn on authentication in the push subscription: Pub/Sub then signs every push as this service account for this audience.
                        </p>
                        <div className="grid gap-3 sm:grid-cols-2">
                          <CopyField label="Authentication: service account" value={urls.pushAccount || "the client_email of google_service_account_json"} />
                          <CopyField label="Authentication: audience" value={urls.pushAudience} />
                        </div>
                      </>
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

type Flags = Pick<GameApp, "enabled" | "allow_sandbox" | "client_consumes" | "account" | "apple_refund_mode" | "sns_topic_arn" | "client_auth">

const REFUND_MODES = [
  { value: "off", label: "Don't answer" },
  { value: "inform", label: "Send consumption data" },
  { value: "prefer", label: "Send data and recommend an outcome" },
]

function DeliveryTab({ data, reload, go }: TabProps) {
  const edit = useAllowed("configure")
  const g = data.group
  const first = g.apps[0]
  const [common, setCommon] = useState<DeliverySettings>({
    delivery: first.delivery, spacetime_db: first.spacetime_db, webhook_url: first.webhook_url,
    spacetime_url: first.spacetime_url || DELIVERY_DEFAULTS.spacetime_url,
    grant_reducer: first.grant_reducer || DELIVERY_DEFAULTS.grant_reducer,
    revoke_reducer: first.revoke_reducer || DELIVERY_DEFAULTS.revoke_reducer,
  })
  const [flags, setFlags] = useState<Record<string, Flags>>(() =>
    Object.fromEntries(g.apps.map((a) => [a.app_id, { enabled: a.enabled, allow_sandbox: a.allow_sandbox, client_consumes: a.client_consumes, account: a.account || "default", apple_refund_mode: a.apple_refund_mode || "off", sns_topic_arn: a.sns_topic_arn, client_auth: a.client_auth || "open" }])))
  const differs = g.apps.some((a) => a.delivery !== first.delivery || a.spacetime_db !== first.spacetime_db || a.webhook_url !== first.webhook_url)

  const save = async () => {
    const ok = await tryAct(async () => {
      for (const a of g.apps) {
        await call("admin_upsert_game_app", JSON.stringify({
          store_app_id: a.store_app_id, platform: a.platform, store: a.store, game_key: g.key, ...common,
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
      {/* Read-only for roles that cannot change builds. A block of its own: the page's space-y spaces
          only its direct children, and a `contents` box shows no margin. */}
      <fieldset disabled={!edit} className="min-w-0 space-y-6">
      <Section title="Reward delivery" description="How purchases reach the app. Shared by all its builds."
        actions={edit && <Button onClick={save}>Save</Button>}>
        {differs && <p className="mb-4 text-sm text-amber-700 dark:text-amber-400">The builds have different settings now; saving applies these to all of them.</p>}
        <DeliveryFields value={common} onChange={(patch) => setCommon((c) => ({ ...c, ...patch }))} />
      </Section>
      <Section title="Builds" description="Each build reads its store keys from its developer account: a game can be published on Google Play, the App Store and the Amazon Appstore from different accounts.">
        <DataTable
          rows={g.apps}
          columns={[
            { title: "Store", cell: (a) => buildLabel(a) },
            { title: "Platform", cell: (a) => platformById(a.platform)?.name || a.platform },
            { title: "App id", cell: (a) => <Mono>{a.store_app_id}</Mono> },
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
            {
              title: "Client requests",
              cell: (a: GameApp) => (
                <Select value={flags[a.app_id]?.client_auth || "open"}
                  onValueChange={(v) => setFlags((f) => ({ ...f, [a.app_id]: { ...f[a.app_id], client_auth: v } }))}>
                  <SelectTrigger size="sm" className="w-44"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="open">Open</SelectItem>
                    <SelectItem value="signed">Signed player tokens</SelectItem>
                  </SelectContent>
                </Select>
              ),
            },
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
            ...(g.apps.some((a) => storeOf(a).id === "amazon") ? [{
              title: "SNS topic",
              cell: (a: GameApp) => storeOf(a).id !== "amazon" ? <span className="text-muted-foreground">-</span> : (
                <Input className="h-8 w-72 font-mono text-xs" placeholder="arn:aws:sns:us-east-1:...:..." value={flags[a.app_id]?.sns_topic_arn || ""}
                  onChange={(e) => setFlags((f) => ({ ...f, [a.app_id]: { ...f[a.app_id], sns_topic_arn: e.target.value.trim() } }))} />
              ),
            }] : []),
            {
              title: "",
              className: "text-right",
              cell: (a) => edit && (
                <span className="inline-flex gap-1">
                  {first.delivery === "webhook" && <Button size="xs" variant="outline" onClick={() => ping(a.app_id)}><WebhookIcon /> Ping</Button>}
                  <Button size="icon-sm" variant="ghost" className="text-destructive" aria-label="Delete build" onClick={() => remove(a)}><Trash2Icon /></Button>
                </span>
              ),
            },
          ]}
        />
        <p className="mt-3 text-xs text-muted-foreground">
          Client requests: open builds accept any player id from the client (use ids nobody can guess); signed builds need a player token from the game's backend on every request (see Client security below).
          Client consumes: the Android client consumes purchases itself (e.g. still through Adapty); otherwise consumed purchases unknown to the ledger are rejected as granted elsewhere.
          SNS topic: Amazon notifications are accepted only from this topic (the setup check shows the topic they come from).
          Refund requests: when an App Store player asks for a refund, Apple asks the server whether the purchase was delivered and used; the answer is sent only for players who consented in the game (<Mono>SetRefundDataConsent</Mono> in the Unity package).
          {edit && "Press Save above to apply."}
        </p>
      </Section>
      </fieldset>
      <ClientSecuritySection apps={g.apps} reload={reload} />
      {g.apps.filter((a) => storeOf(a).id === "steam").map((a) => <SteamBuildSection key={a.app_id} app={a} />)}
      <TransferPolicySection gameKey={g.key} />
      <ServerKeysSection gameKey={g.key} />
    </>
  )
}

/** 32 random bytes from the browser (the module has no secure random source). */
function randomBytes(): Uint8Array {
  const b = new Uint8Array(32)
  crypto.getRandomValues(b)
  return b
}

const base64 = (b: Uint8Array) => btoa(String.fromCharCode(...b))
const hex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("")

/** Player token secret and the response signing key of the app's builds. */
function ClientSecuritySection({ apps, reload }: { apps: GameApp[]; reload: () => void }) {
  const canSet = useCan()("secrets")
  const [shown, setShown] = useState<{ title: string; value: string; note: string } | null>(null)
  const publicKey = useAsync(() => proc<{ public_key: string | null }>("admin_response_public_key", apps[0].app_id), [apps[0].app_id])
  /** Writes the secret to every build; returns the builds it could not be written to. */
  const setAll = async (name: string, value: string) => {
    const failed: string[] = []
    for (const a of apps) {
      try {
        await call("admin_set_credential", a.app_id, name, value)
      } catch {
        failed.push(a.app_id)
      }
    }
    return failed
  }
  const newTokenSecret = async () => {
    if (!confirm("Create a new player token secret for every build of this app? Tokens signed with the old one stop working.")) return
    const secret = hex(randomBytes())
    const failed = await setAll("client_auth_secret", secret)
    // Shown whenever some build has it: the game backend needs it for those builds.
    if (failed.length < apps.length) {
      const partial = failed.length ? ` Not saved for ${failed.join(", ")}: create a new secret again to set it everywhere.` : ""
      setShown({ title: "Player token secret", value: secret, note: "Give it to your game backend: it signs HS256 player tokens {sub: user_id, aud: app, exp} with it. It is not shown again." + partial })
    } else {
      await tryAct(async () => { throw new Error("the secret could not be saved for any build") })
    }
  }
  const newSigningKey = async () => {
    if (!confirm("Create a new response signing key for every build of this app? Games with the old public key stop trusting answers until they are updated.")) return
    const ok = await tryAct(async () => {
      const failed = await setAll("response_signing_key", base64(randomBytes()))
      if (failed.length) throw new Error(`not saved for ${failed.join(", ")}: create a new key again`)
      return true
    }, "Key saved")
    publicKey.reload()
    if (ok) reload()
  }
  return (
    <Section title="Client security" description="Optional protection of the client requests (/verify, /restore, /entitlements, ...). Every client sends plain HTTP: an Authorization header and, for signed answers, a header to check.">
      <div className="space-y-4 text-sm">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="min-w-0">
            <p className="font-medium">Player tokens</p>
            <p className="text-xs text-muted-foreground">For builds with Client requests: signed. Your game backend signs a short-lived token for the player; the client sends <Mono>Authorization: Bearer &lt;token&gt;</Mono>.</p>
          </div>
          {canSet && <Button size="sm" variant="outline" onClick={newTokenSecret}>New token secret</Button>}
        </div>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="min-w-0">
            <p className="font-medium">Signed answers</p>
            <p className="text-xs text-muted-foreground">The server signs /entitlements and /restore answers (<Mono>X-Purser-Signature</Mono>, Ed25519); the game checks them with the public key, so its offline copy cannot be edited.</p>
            {publicKey.data?.public_key && <div className="mt-2 max-w-xl"><CopyField label="Public key (BillingConfig.responseSigningPublicKey)" value={publicKey.data.public_key} /></div>}
          </div>
          {canSet && <Button size="sm" variant="outline" onClick={newSigningKey}>{publicKey.data?.public_key ? "New signing key" : "Create signing key"}</Button>}
        </div>
      </div>
      <Dialog open={!!shown} onOpenChange={(o) => !o && setShown(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{shown?.title}</DialogTitle>
            <DialogDescription>{shown?.note}</DialogDescription>
          </DialogHeader>
          {shown && <CopyField label={shown.title} value={shown.value} />}
          <DialogFooter><Button onClick={() => setShown(null)}>Done</Button></DialogFooter>
        </DialogContent>
      </Dialog>
    </Section>
  )
}

// ---------------------------------------------------------------------------------------------
// Keys: every secret the app needs, and where its value comes from
// ---------------------------------------------------------------------------------------------

/** Secrets usually shared by a developer account; the rest belong to one build. */
const ACCOUNT_SECRETS = new Set([
  ...STORE_SECRETS.filter((s) => s.account_wide).map((s) => s.name),
  ...Object.values(KINDS).filter((k) => k.secretScope === "account").map((k) => k.secret),
  "spacetime_token", "webhook_secret",
])

/** (secret, required) pairs a build needs with its current store, delivery and enabled integrations. */
function neededSecrets(a: GameApp, integrations: AppData["integrations"]): [string, boolean][] {
  const out: [string, boolean][] = storeOf(a).secrets.map((s) => [s.name, s.need !== "optional"])
  if (a.delivery === "spacetimedb") out.push(["spacetime_token", true])
  if (a.delivery === "webhook") out.push(["webhook_secret", true])
  for (const i of integrations) {
    const kind = KINDS[i.kind as keyof typeof KINDS]
    if (kind && i.app_id === a.app_id && i.enabled) out.push([kind.secret, true])
  }
  return out
}

function KeysTab({ data, reload }: TabProps) {
  const g = data.group
  const canSet = useCan()("secrets")
  const allApps = useApps().apps
  const [target, setTarget] = useState<SecretTarget | null>(null)
  const resolve = (name: string, a: GameApp) => {
    for (const [scope, label] of [[a.app_id, "this build"], [a.account, `account ${a.account}`], ["default", "default"]] as const) {
      if (data.creds.has(`${scope}/${name}`)) return label
    }
    return null
  }
  const needed = new Map(g.apps.map((a) => [a.app_id, neededSecrets(a, data.integrations)]))
  const needs = (a: GameApp, name: string) => !!needed.get(a.app_id)?.some(([n]) => n === name)
  const names = new Map<string, boolean>()
  for (const list of needed.values()) {
    for (const [n, req] of list) names.set(n, (names.get(n) || false) || req)
  }
  const rows = [...names.entries()].map(([name, required]) => ({ name, required }))
  const accounts = [...new Set(g.apps.map((a) => a.account || "default"))]
  /** Several developer accounts exist: a store key in `default` would be used by the wrong account's apps. */
  const sharedDefault = data.accounts.length > 0
  const open = (name: string) => {
    const apps = g.apps.filter((a) => needs(a, name))
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
                if (!needs(a, r.name)) return <span className="text-muted-foreground">—</span>
                const from = resolve(r.name, a)
                if (from === "default" && sharedDefault && ACCOUNT_SECRETS.has(r.name)) {
                  return <span title={`This key is in default, shared by every account without its own key. Set it for account ${a.account} if this build is published from its own account.`}><Tone tone="warn">default</Tone></span>
                }
                return from ? <Tone tone="ok">{from}</Tone> : r.required ? <Tone tone="bad">missing</Tone> : <Tone tone="muted">not set</Tone>
              },
            })),
            { title: "", className: "text-right", cell: (r) => canSet && <Button size="xs" variant="outline" onClick={() => open(r.name)}>Set</Button> },
          ]}
        />
      </Section>
      <SecretDialog target={target} onClose={() => setTarget(null)} onSaved={reload} />
    </>
  )
}

interface ServerKeyRow { key_id: string; name: string; scopes: string; created_at_micros: number; last_used_at_micros: number }
const SERVER_SCOPES = [
  ["read", "Read a player's purchases (/server/entitlements)"],
  ["grant", "Give products: rewards, compensation (/server/grant)"],
  ["revoke", "Take purchases back (/server/revoke)"],
  ["wallet", "Read, give and spend virtual currency (/server/wallet)"],
] as const

/** SHA-256 hex in the browser: the server only ever gets the hash of a key. */
async function sha256Hex(text: string): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text))))
}

/** Keys of the app's own backend for the /server/* routes. */
function ServerKeysSection({ gameKey }: { gameKey: string }) {
  const canSet = useCan()("secrets")
  const keys = useAsync(() => select<ServerKeyRow>({ table: "server_key", where: { column: "game_key", equals: gameKey } }), [gameKey])
  const [form, setForm] = useState<{ name: string; scopes: string[] } | null>(null)
  const [shown, setShown] = useState<string | null>(null)
  const create = async () => {
    if (!form) return
    // psk_<key_id>_<secret>: the id is public (listed here), the secret is shown once.
    const keyId = hex(randomBytes()).slice(0, 12)
    const secret = btoa(String.fromCharCode(...randomBytes())).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
    const key = `psk_${keyId}_${secret}`
    const input = { game_key: gameKey, name: form.name.trim() || "backend", scopes: form.scopes.join(","), key_id: keyId, key_hash: await sha256Hex(key) }
    const ok = await tryAct(async () => { await call("admin_add_server_key", JSON.stringify(input)); return true }, "Key created")
    if (ok) {
      setForm(null)
      setShown(key)
      keys.reload()
    }
  }
  const revoke = async (k: ServerKeyRow) => {
    if (!confirm(`Revoke key ${k.key_id} (${k.name})? Requests with it are refused at once.`)) return
    // A reducer's call answers with an empty body: success is "not undefined".
    if ((await tryAct(() => call("admin_revoke_server_key", k.key_id), "Key revoked")) !== undefined) keys.reload()
  }
  return (
    <Section
      title="Server keys"
      description={<>Keys of your game's backend for <Mono>/server/entitlements</Mono>, <Mono>/server/grant</Mono> and <Mono>/server/revoke</Mono> (<Mono>Authorization: Bearer psk_…</Mono>). They work for every build of this app. Never put one in a client.</>}
      actions={canSet && <Button size="sm" variant="outline" onClick={() => setForm({ name: "", scopes: ["read"] })}>New key</Button>}
    >
      {keys.error ? <ErrorBox error={keys.error} /> : keys.loading && !keys.data ? <Loading /> : (
        <DataTable
          rows={keys.data || []}
          empty="No server keys"
          rowKey={(k) => k.key_id}
          columns={[
            { title: "Key id", cell: (k) => <Mono>{k.key_id}</Mono> },
            { title: "Name", cell: (k) => k.name },
            { title: "Scopes", cell: (k) => k.scopes.split(",").join(", ") },
            { title: "Created", cell: (k) => fmtTime(k.created_at_micros) },
            { title: "Last used", cell: (k) => (Number(k.last_used_at_micros) ? fmtTime(k.last_used_at_micros) : "never") },
            { title: "", className: "text-right", cell: (k) => canSet && <Button size="xs" variant="outline" onClick={() => revoke(k)}>Revoke</Button> },
          ]}
        />
      )}
      <Dialog open={!!form} onOpenChange={(o) => !o && setForm(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>New server key</DialogTitle>
            <DialogDescription>The key is made in this browser; the server keeps only its hash.</DialogDescription>
          </DialogHeader>
          {form && (
            <div className="space-y-3">
              <Field label="Name"><Input value={form.name} placeholder="backend" onChange={(e) => setForm({ ...form, name: e.target.value })} /></Field>
              {SERVER_SCOPES.map(([scope, label]) => (
                <label key={scope} className="flex items-center gap-2 text-sm">
                  <Checkbox
                    checked={form.scopes.includes(scope)}
                    onCheckedChange={(on) => setForm({ ...form, scopes: on ? [...form.scopes, scope] : form.scopes.filter((s) => s !== scope) })}
                  />
                  <span><Mono>{scope}</Mono> — {label}</span>
                </label>
              ))}
            </div>
          )}
          <DialogFooter><Button onClick={create} disabled={!form?.scopes.length}>Create</Button></DialogFooter>
        </DialogContent>
      </Dialog>
      <Dialog open={!!shown} onOpenChange={(o) => !o && setShown(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Server key</DialogTitle>
            <DialogDescription>Store it in your backend's secrets. It is not shown again.</DialogDescription>
          </DialogHeader>
          {shown && <CopyField label="Server key" value={shown} />}
          <DialogFooter><Button onClick={() => setShown(null)}>Done</Button></DialogFooter>
        </DialogContent>
      </Dialog>
    </Section>
  )
}

interface TransferPolicy { mode: "always" | "cooldown" | "never"; cooldown_days: number; max_transfers: number }

/** How purchases move between player ids when a restore names another player (`admin_set_transfer_policy`). */
interface SteamBuildRow {
  app_id: string
  steam_app_id: string
  sandbox: boolean
  require_ticket: boolean
}

/** Steam settings of a build (`admin_set_steam_build`): its Steam App ID, the sandbox API, and
 * whether checkouts need the player's Web API auth ticket. */
function SteamBuildSection({ app }: { app: GameApp }) {
  const canSet = useCan()("configure")
  const loaded = useAsync(async () => (await select<SteamBuildRow>({ table: "steam_build", where: { column: "app_id", equals: app.app_id } }))[0] || null, [app.app_id])
  const [edit, setEdit] = useState<SteamBuildRow | null>(null)
  const b = edit || loaded.data || { app_id: app.app_id, steam_app_id: "", sandbox: false, require_ticket: false }
  const set = (patch: Partial<SteamBuildRow>) => setEdit({ ...b, ...patch })
  const save = async () => {
    const ok = await tryAct(() => call("admin_set_steam_build", JSON.stringify(b)), "Saved")
    if (ok !== undefined) { setEdit(null); loaded.reload() }
  }
  return (
    <Section title={`Steam: ${app.store_app_id}`} description="Without an auth ticket, anyone can start a checkout for any SteamID: turn it on once the game sends the ticket."
      actions={canSet && edit ? <Button onClick={save}>Save</Button> : undefined}>
      {loaded.error ? <ErrorBox error={loaded.error} /> : (
        <div className="grid items-start gap-4 sm:grid-cols-3">
          <Field label="Steam App ID" hint="Empty: the build's app_id.">
            <Input inputMode="numeric" value={b.steam_app_id} disabled={!canSet} onChange={(e) => set({ steam_app_id: e.target.value })} />
          </Field>
          <label className="flex items-center gap-2 text-sm">
            <Switch checked={b.require_ticket} disabled={!canSet} onCheckedChange={(v) => set({ require_ticket: v })} /> Require an auth ticket
          </label>
          <label className="flex items-center gap-2 text-sm">
            <Switch checked={b.sandbox} disabled={!canSet} onCheckedChange={(v) => set({ sandbox: v })} /> Sandbox (no money moves)
          </label>
        </div>
      )}
    </Section>
  )
}

function TransferPolicySection({ gameKey }: { gameKey: string }) {
  const canSet = useCan()("configure")
  const loaded = useAsync(() => proc<TransferPolicy>("admin_transfer_policy", gameKey), [gameKey])
  const [edit, setEdit] = useState<TransferPolicy | null>(null)
  const p = edit || loaded.data
  if (loaded.error) return <Section title="Restores on another player"><ErrorBox error={loaded.error} /></Section>
  if (!p) return null
  const set = (patch: Partial<TransferPolicy>) => setEdit({ ...p, ...patch })
  const save = async () => {
    const ok = await tryAct(() => call("admin_set_transfer_policy", gameKey, JSON.stringify(p)), "Saved")
    if (ok !== undefined) { setEdit(null); loaded.reload() }
  }
  return (
    <Section title="Restores on another player" description="Non-consumables and subscriptions follow the store account: a restore on another player id moves them there (the previous player gets revoke). Limit it against account sharing; support can always move a purchase on the player page."
      actions={canSet && edit ? <Button onClick={save}>Save</Button> : undefined}>
      <div className="grid items-start gap-4 sm:grid-cols-3">
        <Field label="Purchases move">
          <Select value={p.mode} disabled={!canSet} onValueChange={(v) => set({ mode: v as TransferPolicy["mode"], cooldown_days: v === "cooldown" ? (p.cooldown_days || 30) : p.cooldown_days })}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="always">Always</SelectItem>
              <SelectItem value="cooldown">At most once per period</SelectItem>
              <SelectItem value="never">Never (support only)</SelectItem>
            </SelectContent>
          </Select>
        </Field>
        {p.mode === "cooldown" && (
          <Field label="Days between moves"><Input type="number" min={1} max={365} disabled={!canSet} value={p.cooldown_days} onChange={(e) => set({ cooldown_days: Number(e.target.value) })} /></Field>
        )}
        {p.mode !== "never" && (
          <Field label="Moves per purchase" hint="0 = no limit"><Input type="number" min={0} disabled={!canSet} value={p.max_transfers} onChange={(e) => set({ max_transfers: Number(e.target.value) })} /></Field>
        )}
      </div>
    </Section>
  )
}
