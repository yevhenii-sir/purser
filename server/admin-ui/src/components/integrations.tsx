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
const ALL_AMOUNTS: Kind[] = ["webhook", "gcs"]
const EVENT_KINDS = ["purchase", "subscription_start", "renewal", "refund", "expire"]
const AF_EVENTS: [string, string][] = [["purchase", "af_purchase"], ["subscription_start", "af_subscribe"], ["renewal", "subscription_renew"], ["refund", "refund"], ["expire", "(off)"]]
const ANY = "__all__"

export function TestEvent({ apps }: { apps: GameApp[] }) {
  const [app, setApp] = useState("")
  const [kind, setKind] = useState(ANY)
  const [user, setUser] = useState("")
  const [ids, setIds] = useState<Record<string, string>>({ appsflyer_id: "", tenjin_id: "", adjust_id: "", appmetrica_device_id: "" })
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
  const [names, setNames] = useState<Record<string, string>>(() =>
    Object.fromEntries(AF_EVENTS.map(([k]) => [k, initial.event_names?.[k] === "" ? "-" : initial.event_names?.[k] ?? ""])))
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
    if (kind === "appsflyer") {
      const event_names: Record<string, string> = {}
      for (const [k] of AF_EVENTS) {
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
              <div className="space-y-2">
                <Label className="text-xs text-muted-foreground">Event names (empty: the default, "-": do not send)</Label>
                <div className="grid gap-3 sm:grid-cols-2">
                  {AF_EVENTS.map(([k, d]) => (
                    <Field key={k} label={k}><Input value={names[k]} placeholder={d} onChange={(e) => setNames((n) => ({ ...n, [k]: e.target.value }))} /></Field>
                  ))}
                </div>
              </div>
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
          {kind === "gcs" && (
            <>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label="Bucket"><Input value={s.bucket || ""} onChange={(e) => set("bucket", e.target.value.trim())} placeholder="my-billing-events" className="font-mono text-xs" /></Field>
                <Field label="Folder"><Input value={s.folder || ""} onChange={(e) => set("folder", e.target.value.trim())} placeholder="billing/events" className="font-mono text-xs" /></Field>
              </div>
              {eventFilter}
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
              {eventFilter}
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

