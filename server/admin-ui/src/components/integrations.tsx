import { useState } from "react"
import { SendIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DialogBody, Field, Section } from "@/components/common"
import { KIND_LIST, KINDS, type Integration, type Kind, type Settings } from "@/components/integration-kinds"
import { SecretField } from "@/components/secret-field"
import { call, genSecret } from "@/lib/api"
import { tryAct } from "@/lib/hooks"
import { isFileSecret } from "@/lib/secrets"
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
/** Kinds whose event names `event_names` overrides, with the default names. */
const EVENT_NAMES: Partial<Record<Kind, [string, string][]>> = {
  appsflyer: [["purchase", "af_purchase"], ["subscription_start", "af_subscribe"], ["renewal", "subscription_renew"], ["refund", "refund"], ["expire", "(off)"]],
  singular: [
    ["purchase", "__iap__"], ["subscription_start", "sng_subscribe / sng_start_trial"], ["renewal", "subscription_renewed"],
    ["refund", "subscription_refunded / refund"], ["cancel", "subscription_cancelled"], ["expire", "(off)"],
  ],
  kochava: [["purchase", "Purchase"], ["subscription_start", "Subscribe / Start Trial"], ["renewal", "Renewal"], ["refund", "Refund"], ["expire", "(off)"]],
}
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
            <SelectContent>{apps.map((a) => <SelectItem key={a.app_id} value={a.app_id}>{a.app_id}</SelectItem>)}</SelectContent>
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

export function IntegrationDialog({ editing, apps, onClose, onSaved }: {
  editing: { kind: Kind; row: Integration | null } | null
  apps: GameApp[]
  onClose: () => void
  onSaved: () => void
}) {
  return (
    <Dialog open={!!editing} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        {editing && <IntegrationForm key={`${editing.kind}/${editing.row?.key}`} kind={editing.kind} row={editing.row} apps={apps} onClose={onClose} onSaved={onSaved} />}
      </DialogContent>
    </Dialog>
  )
}

function IntegrationForm({ kind, row, apps, onClose, onSaved }: {
  kind: Kind
  row: Integration | null
  apps: GameApp[]
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
  const [names, setNames] = useState<Record<string, string>>(() =>
    Object.fromEntries(namedEvents.map(([k]) => [k, initial.event_names?.[k] === "" ? "-" : initial.event_names?.[k] ?? ""])))
  const account = apps.find((a) => a.app_id === app)?.account || "default"
  const set = (k: string, v: unknown) => setS((x) => ({ ...x, [k]: v }))
  const check = (k: string, label: string) => (
    <div className="flex items-center gap-2">
      <Checkbox id={k} checked={!!s[k]} onCheckedChange={(v) => set(k, v === true)} />
      <Label htmlFor={k} className="font-normal">{label}</Label>
    </div>
  )

  const eventFilter = (
    <div className="space-y-2">
      <Label className="text-xs text-muted-foreground">Events (none checked: all)</Label>
      <div className="flex flex-wrap gap-x-5 gap-y-2">
        {EVENT_KINDS.map((e) => {
          const list: string[] = s.events || []
          return (
            <div key={e} className="flex items-center gap-2">
              <Checkbox id={`ev-${e}`} checked={list.includes(e)}
                onCheckedChange={(v) => set("events", v === true ? [...list, e] : list.filter((x) => x !== e))} />
              <Label htmlFor={`ev-${e}`} className="font-normal">{e}</Label>
            </div>
          )
        })}
      </div>
    </div>
  )

  const save = async () => {
    const settings: Settings = { ...s }
    if (namedEvents.length) {
      const event_names: Record<string, string> = {}
      for (const [k] of namedEvents) {
        if (names[k] === "-") event_names[k] = ""
        else if (names[k]) event_names[k] = names[k]
      }
      settings.event_names = event_names
    }
    const ok = await tryAct(async () => {
      await call("admin_upsert_integration", JSON.stringify({ app_id: app, kind, enabled, settings }))
      if (secret.trim()) await call("admin_set_credential", scope === "app" ? app : account, def.secret, secret.trim())
      return true
    }, "Saved")
    if (ok) { onSaved(); onClose() }
  }
  const eventNames = namedEvents.length > 0 && (
    <div className="space-y-2">
      <Label className="text-xs text-muted-foreground">Event names (empty: the default, "-": do not send)</Label>
      <div className="grid gap-3 sm:grid-cols-2">
        {namedEvents.map(([k, d]) => (
          <Field key={k} label={k}><Input value={names[k]} placeholder={d} onChange={(e) => setNames((n) => ({ ...n, [k]: e.target.value }))} /></Field>
        ))}
      </div>
    </div>
  )
  const remove = async () => {
    if (!row || !confirm(`Disconnect ${def.name} from ${row.app_id}? The key stays on the Secrets page.`)) return
    const ok = await tryAct(() => call("admin_delete_integration", row.app_id, row.kind), "Disconnected")
    if (ok !== undefined) { onSaved(); onClose() }
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle className="flex items-center gap-2"><def.icon className="size-4" /> {def.name}</DialogTitle>
        <DialogDescription>{def.description}</DialogDescription>
      </DialogHeader>
      <DialogBody>
        <div className="grid gap-4 pb-1">
          <Field label="App">
            <Select value={app} onValueChange={setApp} disabled={!!row}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{apps.map((a) => <SelectItem key={a.app_id} value={a.app_id}>{a.app_id} ({a.platform})</SelectItem>)}</SelectContent>
            </Select>
          </Field>

          {kind === "ga4" && (
            <Field label="Firebase app id" hint="Firebase console → Project settings → your app, e.g. 1:1234567890:android:abc123. Android and iOS have different ids.">
              <Input value={s.firebase_app_id || ""} onChange={(e) => set("firebase_app_id", e.target.value.trim())} className="font-mono text-xs" />
            </Field>
          )}
          {kind === "appsflyer" && (
            <>
              <Field label="AppsFlyer app id" hint="The package name on Android, id123456789 on iOS.">
                <Input value={s.app_id || ""} onChange={(e) => set("app_id", e.target.value.trim())} className="font-mono text-xs" />
              </Field>
              {eventNames}
            </>
          )}
          {kind === "singular" && (
            <>
              <Field label="App identifier" hint="The package name / bundle id as Singular knows the app. Empty: the build's app id.">
                <Input value={s.app_id || ""} onChange={(e) => set("app_id", e.target.value.trim())} placeholder="com.example.mygame" className="font-mono text-xs" />
              </Field>
              {eventNames}
              <p className="text-xs text-muted-foreground">
                With <code>singular_device_id</code> events go to the V2 endpoint (<code>sdid</code>), otherwise to V1 with the device ids.
                The server does not know the device's IP: Singular places the event by the billing country (<code>use_ip</code> + <code>country</code>).
                Refunds carry a negative amount.
              </p>
            </>
          )}
          {kind === "branch" && (
            <p className="text-xs text-muted-foreground">
              Sent: <code>PURCHASE</code> with revenue (purchases, renewals and paid subscription starts; the kind in <code>customer_event_alias</code>),
              <code> START_TRIAL</code> (free trials), and the custom events <code>purser_refund</code> (the amount in <code>custom_data</code>; Branch takes
              no negative revenue) and <code>purser_expire</code>.
            </p>
          )}
          {kind === "kochava" && (
            <>
              {eventNames}
              <p className="text-xs text-muted-foreground">
                Revenue goes in <code>event_data.sum</code> with <code>currency</code>; refunds carry <code>refund_amount</code> instead.
              </p>
            </>
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
            <>
              <Field label="App token" hint="Adjust → AppView → your app → App token.">
                <Input value={s.app_token || ""} onChange={(e) => set("app_token", e.target.value.trim())} className="font-mono text-xs" />
              </Field>
              <div className="space-y-2">
                <Label className="text-xs text-muted-foreground">Event tokens: create the events in Adjust (with revenue for purchases and renewals). An empty token: the event is not sent.</Label>
                <div className="grid gap-3 sm:grid-cols-2">
                  {[...EVENT_KINDS, "test"].map((k) => (
                    <Field key={k} label={k}>
                      <Input value={s.event_tokens?.[k] || ""} placeholder="not sent" className="font-mono text-xs"
                        onChange={(e) => set("event_tokens", { ...(s.event_tokens || {}), [k]: e.target.value.trim() })} />
                    </Field>
                  ))}
                </div>
              </div>
            </>
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
              <p className="text-xs text-muted-foreground">
                Sent: <code>Purchase</code> (purchases and renewals), <code>Subscribe</code> (paid subscription starts), <code>StartTrial</code> (free trials).
                Meta has no refund event; events older than 7 days are not sent.
              </p>
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

          <div className="rounded-lg bg-muted/50 p-3 text-xs">
            <span className="font-medium">The client sends in <code>attribution</code>: </span>{def.client}
          </div>

          <div className="space-y-3 rounded-lg border p-3">
            {eventFilter}
            {check("skip_client_purchases", "Leave out first purchases of Google Play, App Store and Amazon: the game's SDKs report them")}
            <p className="text-xs text-muted-foreground">
              For games whose Tenjin / Firebase / AppMetrica SDK already reports purchases: renewals, refunds, cancellations
              and purchases through Xsolla, Stripe and Steam still go out from the server, so nothing is counted twice.
            </p>
          </div>

          <div className="flex flex-wrap gap-6">
            {check("send_sandbox", "Send sandbox / test purchases")}
            {kind === "tenjin" && check("send_expire", "Send expirations (custom event subscription_expire)")}
            <div className="flex items-center gap-2">
              <Checkbox id="enabled" checked={enabled} onCheckedChange={(v) => setEnabled(v === true)} />
              <Label htmlFor="enabled" className="font-normal">Enabled</Label>
            </div>
          </div>
        </div>
      </DialogBody>
      <DialogFooter className="gap-2 sm:justify-between">
        <div>{row && <Button variant="ghost" className="text-destructive" onClick={remove}><Trash2Icon /> Disconnect</Button>}</div>
        <Button onClick={save} disabled={!app}>{row ? "Save" : "Connect"}</Button>
      </DialogFooter>
    </>
  )
}

