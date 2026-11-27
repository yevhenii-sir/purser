import { useState, type ReactNode } from "react"
import { SendIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { DialogBody, Field, Section } from "@/components/common"
import { KIND_LIST, KINDS, type Integration, type Kind, type Settings } from "@/components/integration-kinds"
import { SecretField } from "@/components/secret-field"
import { call, genSecret } from "@/lib/api"
import { tryAct } from "@/lib/hooks"
import { isFileSecret } from "@/lib/secrets"
import { buildLabel } from "@/lib/stores"
import { parseJsonOr } from "@/lib/utils"
import type { GameApp } from "@/lib/types"


/** What analytics integrations report as revenue (`revenue` in the settings; the server's `revenue_mode`). */
const REVENUE_MODES = [
  { value: "gross", label: "Gross revenue", hint: "The price the player paid." },
  { value: "proceeds", label: "Proceeds after store commission", hint: "The price minus the estimated store commission (Revenue → Store fee: per app, lower after a subscription's first paid year)." },
  { value: "net", label: "Proceeds after store commission and taxes", hint: "Also without the VAT / GST included in the price, estimated from the buyer's billing country (standard rates; US and Canadian prices exclude tax)." },
]
/** Integrations that get every amount in the event instead of one revenue figure. */
const ALL_AMOUNTS: Kind[] = ["webhook", "gcs", "s3"]
const EVENT_KINDS = ["purchase", "subscription_start", "renewal", "refund", "expire", "cancel", "uncancel", "billing_issue", "billing_recovered", "product_change", "pause", "resume"]
/** Payments and other events of a purchase. */
const PAYMENTS = ["purchase", "subscription_start", "renewal", "refund", "expire"]
/** Subscription changes: sent to product analytics only when named. */
const LIFECYCLE = ["cancel", "uncancel", "billing_issue", "billing_recovered", "product_change", "pause", "resume"]
/** Keys narrower than a kind (the server's `narrow_key`): `[key, its kind, what it is]`. Empty: its kind's name. */
const NARROW_KEYS: [string, string, string][] = [
  ["trial_start", "subscription_start", "A free trial starts"],
  ["trial_converted", "renewal", "The first payment after a free trial"],
  ["trial_cancel", "cancel", "Auto-renew turned off during a free trial"],
  ["trial_uncancel", "uncancel", "Auto-renew turned back on during a free trial"],
  ["trial_expire", "expire", "A free trial ended without a payment"],
  ["subscription_refund", "refund", "A refund of a subscription (refund: of the other products)"],
]
const KIND_OF: Record<string, string> = Object.fromEntries(NARROW_KEYS.map(([k, kind]) => [k, kind]))
const ABOUT: Record<string, string> = Object.fromEntries(NARROW_KEYS.map(([k, , about]) => [k, about]))
/** An event an integration names: `[key, default name ("": its kind's), off by default]`. An event off by
 * default is sent only when named: its switch fills in the default name. */
type NameDef = [string, string, boolean?]
const purser = (keys: string[], off: string[] = []): NameDef[] => keys.map((k) => [k, `purser_${k}`, off.includes(k)])
const narrow = (...keys: string[]): NameDef[] => keys.map((k) => [k, ""])
const ALL_NARROW = NARROW_KEYS.map(([k]) => k)
/** GA4's names of the subscription changes, as other subscription platforms send them. */
const GA4_LIFECYCLE: Record<string, string> = {
  cancel: "subscription_cancel", uncancel: "subscription_uncancel", billing_issue: "billing_issue", billing_recovered: "billing_recovered",
  product_change: "subscription_change", pause: "subscription_pause", resume: "subscription_resume",
}
/** Integrations whose event names `event_names` overrides, with the default names. */
const EVENT_NAMES: Partial<Record<Kind, NameDef[]>> = {
  ga4: [
    ["purchase", "purchase"], ["subscription_start", "purchase"], ["renewal", "purchase"], ["refund", "refund"], ["expire", "subscription_expire"],
    ...LIFECYCLE.map((k): NameDef => [k, GA4_LIFECYCLE[k], true]),
    ["trial_start", "trial_started"], ...narrow("trial_converted", "trial_cancel", "trial_uncancel", "trial_expire", "subscription_refund"),
  ],
  appsflyer: [
    ["purchase", "af_purchase"], ["subscription_start", "af_subscribe"], ["renewal", "subscription_renew"], ["refund", "refund"], ["expire", "subscription_expire", true],
    ...narrow("trial_start", "trial_converted", "trial_expire", "subscription_refund"),
  ],
  singular: [
    ["purchase", "__iap__"], ["subscription_start", "sng_subscribe"], ["renewal", "subscription_renewed"],
    ["refund", "refund"], ["cancel", "subscription_cancelled"], ["expire", "subscription_expired", true],
    ["trial_start", "sng_start_trial"], ...narrow("trial_converted", "trial_cancel", "trial_expire"), ["subscription_refund", "subscription_refunded"],
  ],
  kochava: [
    ["purchase", "Purchase"], ["subscription_start", "Subscribe"], ["renewal", "Renewal"], ["refund", "Refund"], ["expire", "Subscription Expired", true],
    ["trial_start", "Start Trial"], ...narrow("trial_converted", "trial_expire", "subscription_refund"),
  ],
  amplitude: [...purser(EVENT_KINDS, LIFECYCLE), ...narrow(...ALL_NARROW)],
  mixpanel: [...purser(EVENT_KINDS, LIFECYCLE), ...narrow(...ALL_NARROW)],
  onesignal: [...purser(EVENT_KINDS), ...narrow(...ALL_NARROW)],
  braze: [...purser(["expire", ...LIFECYCLE]), ["trial_start", "purser_subscription_start"], ...narrow("trial_cancel", "trial_uncancel", "trial_expire")],
}
/** Settings that name the app in the service, different in every build: copying keeps this build's. */
const BUILD_FIELDS: Partial<Record<Kind, string[]>> = {
  ga4: ["firebase_app_id"], appsflyer: ["app_id"], singular: ["app_id"], braze: ["app_id"], adjust: ["app_token"], appmetrica: ["application_id"],
}
/** What an integration's names must look like, or what to know about them. */
const NAME_HINTS: Partial<Record<Kind, string>> = {
  ga4: "Names: a letter, then letters, digits and _, at most 40. GA4 counts revenue only of purchase and refund events: a payment named otherwise carries no revenue.",
  braze: "Payments go to Braze as purchases; these are the custom events.",
}
/** Kinds an integration without event names can send (the server's builders); the rest send every kind. Adjust
 * has none: its event tokens choose. */
const SENDS: Partial<Record<Kind, string[]>> = {
  adjust: [],
  tenjin: ["purchase", "subscription_start", "renewal", "refund", "expire"],
  appmetrica: [...PAYMENTS, "cancel", "uncancel", "billing_issue"],
  meta: ["purchase", "subscription_start", "renewal"],
  branch: PAYMENTS,
}
/** Kinds an integration with event names sends without a name: the `events` filter still chooses them. */
const UNNAMED_SENT: Partial<Record<Kind, string[]>> = { braze: ["purchase", "subscription_start", "renewal", "refund"] }
const UNNAMED_LABEL: Partial<Record<Kind, string>> = { braze: "Payments sent as Braze purchases" }
/** One event's row: sent or not, and its name ("": the default). */
interface NameRow { on: boolean; name: string }
const ANY = "__all__"

export function TestEvent({ apps }: { apps: GameApp[] }) {
  const [app, setApp] = useState("")
  const [kind, setKind] = useState(ANY)
  const [user, setUser] = useState("")
  const [ids, setIds] = useState<Record<string, string>>({
    appsflyer_id: "", tenjin_id: "", adjust_id: "", appmetrica_device_id: "", amplitude_device_id: "", meta_anon_id: "",
    singular_device_id: "", kochava_device_id: "",
  })
  const target = app || apps[0]?.app_id || ""
  const send = () => {
    const attribution = Object.fromEntries(Object.entries(ids).filter(([, v]) => v.trim()))
    if (ids.tenjin_id) Object.assign(attribution, { os_version: "test" })
    const payload = { app_id: target, kind: kind === ANY ? "" : kind, user_id: user, attribution }
    return tryAct(() => call("admin_test_integration", JSON.stringify(payload)), "Test queued: check Logs in a few seconds")
  }
  return (
    <Section title="Test event" description="An purser_test event without revenue, like Send test event elsewhere. The result shows up on the Logs page and in the service dashboard.">
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <Field label="App">
          <Select value={target} onValueChange={setApp}>
            <SelectTrigger className="w-full"><SelectValue placeholder="Pick an app" /></SelectTrigger>
            <SelectContent>{apps.map((a) => <SelectItem key={a.app_id} value={a.app_id}>{buildLabel(a)} · {a.store_app_id}</SelectItem>)}</SelectContent>
          </Select>
        </Field>
        <Field label="Integration">
          <Select value={kind} onValueChange={setKind}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>All enabled</SelectItem>
              {KIND_LIST.map((k) => <SelectItem key={k} value={k}>{KINDS[k].name}</SelectItem>)}
            </SelectContent>
          </Select>
        </Field>
        <Field label="user_id"><Input value={user} onChange={(e) => setUser(e.target.value)} placeholder="purser-test-user" /></Field>
        {Object.keys(ids).map((k) => (
          <Field key={k} label={k}><Input value={ids[k]} onChange={(e) => setIds({ ...ids, [k]: e.target.value })} placeholder="from a test device" /></Field>
        ))}
      </div>
      <Button className="mt-4" onClick={send} disabled={!target}><SendIcon /> Send test</Button>
    </Section>
  )
}

export function IntegrationDialog({ editing, apps, others = [], onClose, onSaved }: {
  editing: { kind: Kind; row: Integration | null } | null
  apps: GameApp[]
  /** The same integration on the app's other builds, with their labels: the form copies their settings. */
  others?: { label: string; row: Integration }[]
  onClose: () => void
  onSaved: () => void
}) {
  return (
    <Dialog open={!!editing} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-3xl">
        {editing && <IntegrationForm key={`${editing.kind}/${editing.row?.key}`} kind={editing.kind} row={editing.row} apps={apps} others={others} onClose={onClose} onSaved={onSaved} />}
      </DialogContent>
    </Dialog>
  )
}

function IntegrationForm({ kind, row, apps, others, onClose, onSaved }: {
  kind: Kind
  row: Integration | null
  apps: GameApp[]
  others: { label: string; row: Integration }[]
  onClose: () => void
  onSaved: () => void
}) {
  const def = KINDS[kind]
  const initial = parseJsonOr<Settings>(row?.settings_json, {})
  const [app, setApp] = useState(row?.app_id || apps[0]?.app_id || "")
  const [s, setS] = useState<Settings>(initial)
  const [enabled, setEnabled] = useState(!row || row.enabled)
  const [secret, setSecret] = useState("")
  const [scope, setScope] = useState<"app" | "account">(def.secretScope)
  const namedEvents = EVENT_NAMES[kind] || []
  /** Kinds an integration with event names sends without a name (Braze purchases): ticked = sent. */
  const unnamed = UNNAMED_SENT[kind] || []
  // With event names the switches choose the events; a kind the `events` filter left out starts off.
  const filteredOut = (from: Settings, k: string) => !!from.events?.length && !from.events.includes(KIND_OF[k] || k)
  const namesOf = (from: Settings) =>
    Object.fromEntries(namedEvents.map(([k, , off]): [string, NameRow] => {
      const saved = from.event_names?.[k]
      if (saved === "" || (filteredOut(from, k) && !saved)) return [k, { on: false, name: "" }]
      // A narrow key without a name follows its kind, so it starts on.
      return [k, { on: !!saved || !off, name: saved || "" }]
    }))
  const [names, setNames] = useState<Record<string, NameRow>>(() => namesOf(initial))
  /** Kinds the checkboxes choose: the unnamed ones of an integration with names, else what it can send. */
  const checkable = namedEvents.length ? unnamed : SENDS[kind] ?? EVENT_KINDS
  // Tenjin sends expirations only with `send_expire`: its checkbox is that setting.
  const offOf = (from: Settings) => checkable.filter((k) => (kind === "tenjin" && k === "expire" ? !from.send_expire : filteredOut(from, k)))
  const [kindsOff, setKindsOff] = useState<string[]>(() => offOf(initial))
  const account = apps.find((a) => a.app_id === app)?.account || "default"
  const set = (k: string, v: unknown) => setS((x) => ({ ...x, [k]: v }))
  const check = (k: string, label: string) => (
    <div className="flex items-center gap-2">
      <Checkbox id={k} checked={!!s[k]} onCheckedChange={(v) => set(k, v === true)} />
      <Label htmlFor={k} className="font-normal">{label}</Label>
    </div>
  )

  const save = async () => {
    const settings: Settings = { ...s }
    if (namedEvents.length) {
      const event_names: Record<string, string> = {}
      for (const [k, d, off] of namedEvents) {
        const { on, name } = names[k]
        const trimmed = name.trim()
        if (!on) event_names[k] = ""
        else if (trimmed) event_names[k] = trimmed
        // The server sends an event that is off by default only when it is named.
        else if (off) event_names[k] = d
      }
      settings.event_names = event_names
      // The switches replace the `events` filter; only kinds sent without a name can still be left out.
      settings.events = kindsOff.length ? EVENT_KINDS.filter((k) => !kindsOff.includes(k)) : []
    } else if (!checkable.length) {
      settings.events = []
    } else {
      if (kindsOff.length === checkable.length) {
        toast.error("Every event is unchecked: check one, or turn the integration off")
        return
      }
      settings.events = kindsOff.length ? checkable.filter((k) => !kindsOff.includes(k)) : []
      if (kind === "tenjin") settings.send_expire = !kindsOff.includes("expire")
    }
    const ok = await tryAct(async () => {
      await call("admin_upsert_integration", JSON.stringify({ app_id: app, kind, enabled, settings }))
      if (secret.trim()) await call("admin_set_credential", scope === "app" ? app : account, def.secret, secret.trim())
      return true
    }, "Saved")
    if (ok) { onSaved(); onClose() }
  }
  // Rows that differ from the integration's defaults.
  const changed = namedEvents.filter(([k, d, off]) => (names[k].name.trim() && names[k].name.trim() !== d) || names[k].on === !!off).length
  // A new integration starts with its connection; a connected one with its events, what is changed later.
  const [tab, setTab] = useState<"connection" | "events">(row ? "events" : "connection")
  /** Takes the settings of the same integration on another build, keeping this build's ids; never the key. */
  const copyFrom = (key: string) => {
    const from = others.find((o) => o.row.key === key)
    if (!from) return
    const copied = parseJsonOr<Settings>(from.row.settings_json, {})
    const kept = BUILD_FIELDS[kind] || []
    for (const f of kept) {
      if (s[f] === undefined) delete copied[f]
      else copied[f] = s[f]
    }
    setS(copied)
    setNames(namesOf(copied))
    setKindsOff(offOf(copied))
    toast.success(`Copied from ${from.label}${kept.length ? `; ${kept.join(", ")} kept` : ""}. Save to apply; the key is not copied.`)
  }
  const setRow = (k: string, row: Partial<NameRow>) => setNames((n) => ({ ...n, [k]: { ...n[k], ...row } }))
  const nameField = ([k, d, off]: NameDef) => {
    const { on, name } = names[k]
    const placeholder = d || `as ${KIND_OF[k]}`
    // Turned on, an event that is off by default gets its default name to keep or change.
    const toggle = (v: boolean) => setRow(k, v && off && !name.trim() ? { on: v, name: d } : { on: v })
    return (
      <Field key={k} label={k} hint={ABOUT[k]}>
        <div className="flex items-center gap-2">
          <Switch checked={on} onCheckedChange={toggle} aria-label={`Send ${k}`} />
          <Input value={name} disabled={!on} placeholder={on ? placeholder : "not sent"} className="font-mono text-xs"
            onChange={(e) => setRow(k, { name: e.target.value })} />
        </div>
      </Field>
    )
  }
  const remove = async () => {
    if (!row || !confirm(`Disconnect ${def.name} from ${row.app_id}? The key stays on the Secrets page.`)) return
    const ok = await tryAct(() => call("admin_delete_integration", row.app_id, row.kind), "Disconnected")
    if (ok !== undefined) { onSaved(); onClose() }
  }
  const kindBox = (e: string) => (
    <div key={e} className="flex min-w-0 items-center gap-2">
      <Checkbox id={`ev-${e}`} checked={!kindsOff.includes(e)}
        onCheckedChange={(v) => setKindsOff((off) => (v === true ? off.filter((x) => x !== e) : [...off, e]))} />
      <Label htmlFor={`ev-${e}`} className="truncate font-normal">{e}</Label>
    </div>
  )
  const kindGroups = checkable.length > 0 && (
    <div className="grid gap-3">
      {[["Payments", PAYMENTS], ["Subscription changes", LIFECYCLE]].map(([title, kinds]) => {
        const shown = checkable.filter((k) => (kinds as string[]).includes(k))
        return shown.length > 0 && (
          <div key={title as string} className="grid gap-2">
            <p className="text-xs text-muted-foreground">{title}</p>
            <div className="grid grid-cols-2 gap-x-4 gap-y-2 sm:grid-cols-3 md:grid-cols-4">{shown.map(kindBox)}</div>
          </div>
        )
      })}
    </div>
  )
  const heading = (title: ReactNode, note?: ReactNode) => (
    <div className="space-y-1">
      <h3 className="text-sm font-medium">{title}</h3>
      {note && <p className="text-xs text-muted-foreground">{note}</p>}
    </div>
  )
  const grid = "grid gap-3 sm:grid-cols-2 md:grid-cols-3"

  const connection = (
    <div className="grid gap-5">
      <div className={others.length ? "grid items-start gap-4 sm:grid-cols-2" : "grid"}>
        <Field label="App">
          <Select value={app} onValueChange={setApp} disabled={!!row}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{apps.map((a) => <SelectItem key={a.app_id} value={a.app_id}>{buildLabel(a)} · {a.store_app_id}</SelectItem>)}</SelectContent>
          </Select>
        </Field>
        {others.length > 0 && (
          <Field label="Copy settings from" hint="Event names and options of another build; this build's ids and key stay.">
            <Select value="" onValueChange={copyFrom}>
              <SelectTrigger className="w-full"><SelectValue placeholder="Another build" /></SelectTrigger>
              <SelectContent>{others.map((o) => <SelectItem key={o.row.key} value={o.row.key}>{o.label}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
        )}
      </div>

      {kind === "ga4" && (
        <Field label="Firebase app id" hint="Firebase console → Project settings → your app, e.g. 1:1234567890:android:abc123. Android and iOS have different ids.">
          <Input value={s.firebase_app_id || ""} onChange={(e) => set("firebase_app_id", e.target.value.trim())} className="font-mono text-xs" />
        </Field>
      )}
      {kind === "appsflyer" && (
        <Field label="AppsFlyer app id" hint="The package name on Android, id123456789 on iOS.">
          <Input value={s.app_id || ""} onChange={(e) => set("app_id", e.target.value.trim())} className="font-mono text-xs" />
        </Field>
      )}
      {kind === "singular" && (
        <Field label="App identifier" hint="The package name / bundle id as Singular knows the app. Empty: the build's app id.">
          <Input value={s.app_id || ""} onChange={(e) => set("app_id", e.target.value.trim())} placeholder="com.example.mygame" className="font-mono text-xs" />
        </Field>
      )}
      {kind === "braze" && (
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label="REST endpoint" hint="Your instance's REST endpoint (Settings → APIs and Identifiers).">
            <Input value={s.endpoint || ""} onChange={(e) => set("endpoint", e.target.value.trim())} placeholder="https://rest.iad-01.braze.com" className="font-mono text-xs" />
          </Field>
          <Field label="App identifier" hint="Optional: the API identifier of the app the purchases belong to.">
            <Input value={s.app_id || ""} onChange={(e) => set("app_id", e.target.value.trim())} className="font-mono text-xs" />
          </Field>
        </div>
      )}
      {kind === "onesignal" && (
        <Field label="App ID" hint="OneSignal → Settings → Keys & IDs. Custom Events need a paid plan.">
          <Input value={s.app_id || ""} onChange={(e) => set("app_id", e.target.value.trim())} placeholder="00000000-0000-0000-0000-000000000000" className="font-mono text-xs" />
        </Field>
      )}
      {kind === "s3" && (
        <>
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Endpoint" hint="https://host[:port] of the S3 API, without the bucket.">
              <Input value={s.endpoint || ""} onChange={(e) => set("endpoint", e.target.value.trim())} placeholder="https://s3.us-east-1.amazonaws.com" className="font-mono text-xs" />
            </Field>
            <Field label="Region" hint="The bucket's region; auto for Cloudflare R2. Empty: us-east-1.">
              <Input value={s.region || ""} onChange={(e) => set("region", e.target.value.trim())} placeholder="us-east-1" className="font-mono text-xs" />
            </Field>
            <Field label="Bucket"><Input value={s.bucket || ""} onChange={(e) => set("bucket", e.target.value.trim())} placeholder="my-billing-events" className="font-mono text-xs" /></Field>
            <Field label="Prefix"><Input value={s.prefix || ""} onChange={(e) => set("prefix", e.target.value.trim())} placeholder="billing/events" className="font-mono text-xs" /></Field>
          </div>
          <p className="text-xs text-muted-foreground">
            Objects: <code>{s.prefix ? `${s.prefix}/` : ""}dt=YYYY-MM-DD/&lt;time&gt;-&lt;event&gt;-&lt;transaction&gt;.json</code>, one JSON line each,
            uploaded with a path-style <code>PUT</code> signed with Signature V4 (AWS S3, Cloudflare R2: <code>https://&lt;account id&gt;.r2.cloudflarestorage.com</code>, MinIO and others).
          </p>
        </>
      )}
      {kind === "adjust" && (
        <Field label="App token" hint="Adjust → AppView → your app → App token.">
          <Input value={s.app_token || ""} onChange={(e) => set("app_token", e.target.value.trim())} className="font-mono text-xs" />
        </Field>
      )}
      {kind === "appmetrica" && (
        <>
          <Field label="Application id" hint="AppMetrica → your app → Settings: the numeric id.">
            <Input value={s.application_id || ""} onChange={(e) => set("application_id", e.target.value.trim())} className="font-mono text-xs" />
          </Field>
          {check("use_profile_id", "Match players by profile id: the game calls SetUserProfileID with the billing user id")}
        </>
      )}
      {kind === "amplitude" && (
        <Field label="Region" hint="EU: a project in Amplitude's EU data center (api.eu.amplitude.com).">
          <Select value={s.region || "us"} onValueChange={(v) => set("region", v)}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="us">US (default)</SelectItem>
              <SelectItem value="eu">EU</SelectItem>
            </SelectContent>
          </Select>
        </Field>
      )}
      {kind === "mixpanel" && (
        <div className="grid items-start gap-4 sm:grid-cols-2">
          <Field label="Project id" hint="Mixpanel → Project settings → Overview: the numeric id.">
            <Input value={s.project_id || ""} onChange={(e) => set("project_id", e.target.value.trim())} className="font-mono text-xs" />
          </Field>
          <Field label="Data residency">
            <Select value={s.region || "us"} onValueChange={(v) => set("region", v)}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="us">US (default)</SelectItem>
                <SelectItem value="eu">EU</SelectItem>
                <SelectItem value="in">India</SelectItem>
              </SelectContent>
            </Select>
          </Field>
        </div>
      )}
      {kind === "meta" && (
        <>
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Dataset id" hint="Events Manager → the app's dataset (pixel): the numeric id.">
              <Input value={s.dataset_id || ""} onChange={(e) => set("dataset_id", e.target.value.trim())} className="font-mono text-xs" />
            </Field>
            <Field label="Graph API version">
              <Input value={s.graph_version || ""} onChange={(e) => set("graph_version", e.target.value.trim())} placeholder="v23.0" className="font-mono text-xs" />
            </Field>
          </div>
          <Field label="Test event code" hint="Events Manager → Test events. While set, every event goes to Test events only; clear it for production.">
            <Input value={s.test_event_code || ""} onChange={(e) => set("test_event_code", e.target.value.trim())} placeholder="TEST12345" className="font-mono text-xs" />
          </Field>
        </>
      )}
      {kind === "gcs" && (
        <>
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Bucket"><Input value={s.bucket || ""} onChange={(e) => set("bucket", e.target.value.trim())} placeholder="my-billing-events" className="font-mono text-xs" /></Field>
            <Field label="Folder"><Input value={s.folder || ""} onChange={(e) => set("folder", e.target.value.trim())} placeholder="billing/events" className="font-mono text-xs" /></Field>
          </div>
          <p className="text-xs text-muted-foreground">
            Files: <code>{s.folder ? `${s.folder}/` : ""}dt=YYYY-MM-DD/&lt;time&gt;-&lt;event&gt;-&lt;transaction&gt;.json</code>, one JSON line each.
            In BigQuery: an external table over <code>gs://{s.bucket || "bucket"}/{s.folder ? `${s.folder}/` : ""}*</code> with format NEWLINE_DELIMITED_JSON and hive partitioning on <code>dt</code>.
          </p>
        </>
      )}
      {kind === "webhook" && (
        <>
          <Field label="URL (https)">
            <Input value={s.url || ""} onChange={(e) => set("url", e.target.value.trim())} placeholder="https://example.com/billing-events" className="font-mono text-xs" />
          </Field>
          <p className="text-xs text-muted-foreground">
            POST with the event as JSON and the headers <code>X-Billing-Event</code> and <code>X-Billing-Signature: hex(HMAC-SHA256(key, body))</code>.
          </p>
        </>
      )}

      <div className="rounded-lg border p-3">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <Label>{def.secretLabel}</Label>
          <Select value={scope} onValueChange={(v) => setScope(v as "app" | "account")}>
            <SelectTrigger size="sm" className="w-56"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="app">For this app</SelectItem>
              <SelectItem value="account">For account {account}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        {isFileSecret(def.secret) ? (
          <SecretField file accept=".json,application/json" value={secret} onChange={setSecret}
            placeholder={row ? "Leave empty to keep the stored key" : "Paste the JSON key or pick the file"} />
        ) : <div className="flex gap-2">
          <SecretField value={secret} onChange={setSecret} placeholder={row ? "Leave empty to keep the stored key" : "Paste the key"} />
          {kind === "webhook" && <Button variant="outline" onClick={() => { const k = genSecret(); setSecret(k); navigator.clipboard?.writeText(k).then(() => toast.success("Generated and copied: give it to the endpoint")) }}>Generate</Button>}
        </div>}
        <p className="mt-2 text-xs text-muted-foreground">Stored as <code>{def.secret}</code>; it is never shown again.</p>
      </div>

      <p className="text-xs text-muted-foreground">
        <span className="font-medium text-foreground">The client sends in <code>attribution</code>: </span>{def.client}
      </p>
    </div>
  )

  const events = (
    <div className="grid gap-6">
      <div className="grid gap-3">
        {ALL_AMOUNTS.includes(kind) ? (
          <p className="text-xs text-muted-foreground">
            Every event carries <code>price_micros</code> (gross) and the estimates <code>proceeds_micros</code> (after store commission)
            and <code>net_micros</code> (after store commission and taxes), with <code>country</code> and <code>commission_percent</code>.
          </p>
        ) : (
          <Field label="How the revenue is sent" hint={REVENUE_MODES.find((m) => m.value === (s.revenue || "gross"))?.hint}>
            <Select value={s.revenue || "gross"} onValueChange={(v) => set("revenue", v)}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{REVENUE_MODES.map((m) => <SelectItem key={m.value} value={m.value}>{m.label}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
        )}
        {kind === "singular" && (
          <p className="text-xs text-muted-foreground">
            With <code>singular_device_id</code> events go to the V2 endpoint (<code>sdid</code>), otherwise to V1 with the device ids.
            The server does not know the device's IP: Singular places the event by the billing country (<code>use_ip</code> + <code>country</code>).
            Refunds carry a negative amount.
          </p>
        )}
        {kind === "branch" && (
          <p className="text-xs text-muted-foreground">
            Sent: <code>PURCHASE</code> with revenue (purchases, renewals and paid subscription starts; the kind in <code>customer_event_alias</code>),
            <code> START_TRIAL</code> (free trials), and the custom events <code>purser_refund</code> (the amount in <code>custom_data</code>; Branch takes
            no negative revenue) and <code>purser_expire</code>.
          </p>
        )}
        {kind === "kochava" && (
          <p className="text-xs text-muted-foreground">
            Revenue goes in <code>event_data.sum</code> with <code>currency</code>; refunds carry <code>refund_amount</code> instead.
          </p>
        )}
        {kind === "meta" && (
          <p className="text-xs text-muted-foreground">
            Sent: <code>Purchase</code> (purchases and renewals), <code>Subscribe</code> (paid subscription starts), <code>StartTrial</code> (free trials).
            Meta has no refund event; events older than 7 days are not sent.
          </p>
        )}
      </div>

      {checkable.length > 0 && (
        <div className="grid gap-3">
          {heading("What is sent", UNNAMED_LABEL[kind] ? `${UNNAMED_LABEL[kind]}: unchecked are not sent.` : "Unchecked events are not sent.")}
          {kindGroups}
        </div>
      )}

      <div className="grid gap-3">
        {heading("Options")}
        <div className="space-y-1">
          {check("skip_client_purchases", "Leave out first purchases of Google Play, App Store and Amazon: the game's SDKs report them")}
          <p className="pl-6 text-xs text-muted-foreground">
            For games whose Tenjin / Firebase / AppMetrica SDK already reports purchases: renewals, refunds, cancellations
            and purchases through Xsolla, Stripe and Steam still go out from the server, so nothing is counted twice.
          </p>
        </div>
        <div className="flex flex-wrap gap-x-6 gap-y-3">
          {check("send_sandbox", "Send sandbox / test purchases")}
        </div>
        {kind === "ga4" && (
          <div className="space-y-1">
            {check("send_user_id", "Send the player's user id as GA4 user_id")}
            <p className="pl-6 text-xs text-muted-foreground">
              Only when the game gives Firebase the same id (FirebaseAnalytics.SetUserId): otherwise GA4 sees one player as two users.
            </p>
          </div>
        )}
      </div>

      {kind === "adjust" && (
        <div className="grid gap-3">
          {heading("Event tokens", "Create the events in Adjust (with revenue for purchases and renewals). An empty token: the event is not sent.")}
          <div className={grid}>
            {[...EVENT_KINDS, "test", ...ALL_NARROW].map((k) => (
              <Field key={k} label={k} hint={ABOUT[k]}>
                <Input value={s.event_tokens?.[k] || ""} placeholder={KIND_OF[k] ? `${KIND_OF[k]}'s token` : "not sent"} className="font-mono text-xs"
                  onChange={(e) => set("event_tokens", { ...(s.event_tokens || {}), [k]: e.target.value.trim() })} />
              </Field>
            ))}
          </div>
        </div>
      )}

      {namedEvents.length > 0 && (
        <div className="grid gap-3">
          {heading(
            <>Event names{changed > 0 && <span className="font-normal text-muted-foreground"> · {changed} changed</span>}</>,
            <>
              The switch sends the event or not; an empty name sends it under the default shown in the field. Subscription changes are off until switched on.
              {NAME_HINTS[kind] && <> {NAME_HINTS[kind]}</>}
            </>,
          )}
          <div className={grid}>{namedEvents.filter(([k]) => !KIND_OF[k]).map(nameField)}</div>
          {namedEvents.some(([k]) => KIND_OF[k]) && (
            <>
              <p className="pt-2 text-xs font-medium">
                Free trials and subscription refunds <span className="font-normal text-muted-foreground">(empty: the name of the event they belong to)</span>
              </p>
              <div className={grid}>{namedEvents.filter(([k]) => KIND_OF[k]).map(nameField)}</div>
            </>
          )}
        </div>
      )}
    </div>
  )

  return (
    <>
      <DialogHeader>
        <div className="flex items-center gap-3 pr-8">
          <DialogTitle className="flex min-w-0 flex-1 items-center gap-2"><def.icon className="size-4 shrink-0" /> <span className="truncate">{def.name}</span></DialogTitle>
          <Label htmlFor="enabled" className="shrink-0 text-xs font-normal text-muted-foreground">{enabled ? "Enabled" : "Disabled"}</Label>
          <Switch id="enabled" checked={enabled} onCheckedChange={setEnabled} />
        </div>
        <DialogDescription>{def.description}</DialogDescription>
      </DialogHeader>
      <Tabs value={tab} onValueChange={(t) => setTab(t as "connection" | "events")}>
        <TabsList variant="line" className="h-auto w-full justify-start gap-x-1 border-b pb-1">
          <TabsTrigger value="connection" className="flex-none px-2">Connection</TabsTrigger>
          <TabsTrigger value="events" className="flex-none px-2">Events</TabsTrigger>
        </TabsList>
      </Tabs>
      <DialogBody className="h-[min(60svh,38rem)]">
        <div className="pb-1">{tab === "connection" ? connection : events}</div>
      </DialogBody>
      <DialogFooter className="gap-2 sm:justify-between">
        <div>{row && <Button variant="ghost" className="text-destructive" onClick={remove}><Trash2Icon /> Disconnect</Button>}</div>
        <Button onClick={save} disabled={!app}>{row ? "Save" : "Connect"}</Button>
      </DialogFooter>
    </>
  )
}

