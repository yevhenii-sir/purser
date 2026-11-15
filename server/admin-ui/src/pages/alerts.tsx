import { useMemo, useState } from "react"
import { PencilIcon, PlusIcon, SendIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { AppsPicker } from "@/components/apps-picker"
import { RichTable } from "@/components/rich-table"
import { SecretField } from "@/components/secret-field"
import { call, getSetting, proc, select } from "@/lib/api"
import { useAppIndex, useApps } from "@/lib/apps"
import { buildLabel } from "@/lib/stores"
import { errorText, fmtTime } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { parseJsonOr } from "@/lib/utils"

type ChannelKind = "telegram" | "slack" | "discord" | "webhook"

/** An `alert_channel` row, with defaults for rows of older servers (one channel per kind, no filters). */
interface Channel {
  id: number
  name: string
  kind: ChannelKind
  enabled: boolean
  settings_json: string
  /** App keys; empty = every app and the server-wide alerts. */
  gameKeys: string[]
  /** Rule ids; empty = every rule. */
  rules: string[]
}

interface ChannelRow {
  id?: number
  name?: string
  kind: ChannelKind
  enabled: boolean
  settings_json: string
  game_keys?: string
  rules?: string
}

const listOf = (s: string | undefined) => (s || "").split(",").map((x) => x.trim()).filter(Boolean)

const toChannel = (r: ChannelRow): Channel => ({
  id: Number(r.id ?? 0),
  name: r.name || r.kind,
  kind: r.kind,
  enabled: !!r.enabled,
  settings_json: r.settings_json,
  gameKeys: listOf(r.game_keys),
  rules: listOf(r.rules),
})

/** Credential scope of a channel's secret (`alerts::secret_scope`). */
const secretScope = (id: number) => `alert-${id}`

/** Messages a channel gets per window, and the window (`alerts::FLOOD_CAP`, `FLOOD_WINDOW_MICROS`). */
const FLOOD_CAP = 10
const FLOOD_MINUTES = 10

interface AlertState {
  key: string
  rule: string
  app_id: string
  active: boolean
  first_at_micros: number
  last_at_micros: number
  suppressed: number
  total: number
  detail: string
}

interface AlertLog {
  id: number
  at_micros: number
  rule: string
  state: string
  app_id: string
  title: string
  detail: string
  repeats: number
  results_json: string
}

/** Mirrors `purser_core::alerts::Rules` (defaults included). */
interface Rules {
  delivery_failing: boolean
  delivery_attempts: number
  store_auth: boolean
  integration_failed: boolean
  notifications_silent: boolean
  silent_days: number
  notification_rejected: boolean
  fx_failing: boolean
  fx_failing_hours: number
  window_hours: number
  send_resolved: boolean
}

const DEFAULT_RULES: Rules = {
  delivery_failing: true,
  delivery_attempts: 3,
  store_auth: true,
  integration_failed: true,
  notifications_silent: true,
  silent_days: 3,
  notification_rejected: true,
  fx_failing: true,
  fx_failing_hours: 24,
  window_hours: 6,
  send_resolved: true,
}

interface KindDef {
  kind: ChannelKind
  title: string
  secret: string
  secretLabel: string
  secretHint: string
  /** Webhook: the signing key is optional; the others cannot send without their secret. */
  secretOptional?: boolean
  setting?: { key: "chat_id" | "url"; label: string; placeholder: string; hint: string }
}

const CHANNEL_KINDS: KindDef[] = [
  {
    kind: "telegram",
    title: "Telegram",
    secret: "alert_telegram_bot_token",
    secretLabel: "Bot token",
    secretHint: "From @BotFather (123456789:AA…). Add the bot to the chat or group first.",
    setting: { key: "chat_id", label: "Chat id", placeholder: "-1001234567890 or @channel_name", hint: "Send a message to the bot, then open api.telegram.org/bot<token>/getUpdates to see the chat id." },
  },
  {
    kind: "slack",
    title: "Slack",
    secret: "alert_slack_webhook_url",
    secretLabel: "Incoming webhook URL",
    secretHint: "Slack app → Incoming Webhooks → https://hooks.slack.com/services/…",
  },
  {
    kind: "discord",
    title: "Discord",
    secret: "alert_discord_webhook_url",
    secretLabel: "Webhook URL",
    secretHint: "Channel settings → Integrations → Webhooks → Copy URL.",
  },
  {
    kind: "webhook",
    title: "Webhook (JSON)",
    secret: "alert_webhook_secret",
    secretLabel: "Signing key (optional)",
    secretHint: "Signs the body: X-Billing-Signature = hex(HMAC-SHA256(key, body)).",
    secretOptional: true,
    setting: { key: "url", label: "URL", placeholder: "https://ops.example.com/billing-alerts", hint: "Receives a JSON POST per alert." },
  },
]

const kindDef = (kind: string) => CHANNEL_KINDS.find((k) => k.kind === kind) || CHANNEL_KINDS[0]

interface Threshold {
  key: keyof Rules
  label: string
  min: number
  max: number
}

const WINDOW: Threshold = { key: "window_hours", label: "Hours", min: 1, max: 168 }

/** The rules; `key` is also the rule id (`purser_core::alerts::rule`) a channel filters by. */
const RULES: { key: keyof Rules; title: string; description: string; threshold?: Threshold }[] = [
  {
    key: "delivery_failing",
    title: "Purchase delivery keeps failing",
    description: "Grant / revoke delivery to the game, or Google consume / acknowledge, failed several times in a row. Resolved when nothing is stuck any more.",
    threshold: { key: "delivery_attempts", label: "Failed attempts", min: 1, max: 50 },
  },
  { key: "store_auth", title: "Store rejected the credentials", description: "Google Play 401/403 or a rejected service account key, App Store 401/403. One alert per developer account, however many of its builds see it. Resolved by the next successful verification." },
  { key: "integration_failed", title: "Integration event dropped", description: "An event to GA4, AppsFlyer, Adjust, a webhook, … failed for good (non-retryable error or out of retries)." },
  {
    key: "notifications_silent",
    title: "No store notifications",
    description: "An app sold something but no RTDN / App Store notification arrived for this many days (production purchases only).",
    threshold: { key: "silent_days", label: "Days", min: 1, max: 60 },
  },
  { key: "notification_rejected", title: "Store notification rejected", description: "A notification came with a wrong or missing secret in its URL." },
  {
    key: "fx_failing",
    title: "Exchange rates are not updating",
    description: "The ECB rates could not be fetched for this many hours (revenue in USD).",
    threshold: { key: "fx_failing_hours", label: "Hours", min: 1, max: 720 },
  },
]

const ruleTitle = (id: string) => RULES.find((r) => r.key === id)?.title || id

export function AlertsPage() {
  const { data, error, loading, reload } = useAsync(async () => {
    const [channels, creds, rules, states, log] = await Promise.all([
      select<ChannelRow>({ table: "alert_channel" }),
      select<{ key: string }>({ table: "credential", columns: ["key"] }),
      getSetting("alert_rules"),
      select<AlertState>({ table: "alert_state" }),
      select<AlertLog>({ table: "alert_log" }),
    ])
    // A broken document: the server falls back to the defaults as well.
    const parsed = parseJsonOr<Partial<Rules>>(rules, {})
    return {
      channels: channels.map(toChannel).sort((a, b) => a.name.localeCompare(b.name)),
      secrets: new Set(creds.map((c) => c.key)),
      rules: { ...DEFAULT_RULES, ...parsed },
      active: states.filter((s) => s.active || Number(s.suppressed) > 0).sort((a, b) => Number(b.last_at_micros) - Number(a.last_at_micros)),
      log: log.sort((a, b) => Number(b.id) - Number(a.id)).slice(0, 100),
    }
  })
  /** The channel being edited, "new" for one being added. */
  const [editing, setEditing] = useState<Channel | "new" | null>(null)
  const hasSecret = (c: Channel) => !!data?.secrets.has(`${secretScope(c.id)}/${kindDef(c.kind).secret}`)
  return (
    <>
      <PageHeader
        title="Alerts"
        description={
          <>
            Messages to Telegram, Slack, Discord or your own webhook when something needs attention, so you do not have to watch this page.
            Add several channels of a kind to send different apps or problems to different chats: an app filter limits a channel to those apps
            (server-wide alerts such as exchange rates go only to channels without one), a rule filter to some problems.
            At most one message per problem and app per window; repeats are counted into the next one. A channel gets at
            most {FLOOD_CAP} messages per {FLOOD_MINUTES} minutes; the rest is summed up in one message when the {FLOOD_MINUTES} minutes end.
          </>
        }
        actions={data && <Button onClick={() => setEditing("new")}><PlusIcon /> Add channel</Button>}
      />
      {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={6} /> : (
        <>
          <Section title="Channels" description="Where alerts go. A channel without filters gets every alert.">
            <ChannelList channels={data.channels} hasSecret={hasSecret} onEdit={setEditing} reload={reload} />
          </Section>
          <RulesSection key={JSON.stringify(data.rules)} initial={data.rules} reload={reload} />
          <OpenProblems active={data.active} reload={reload} />
          <Section title="Recent alerts" description="The last 100 messages and what each channel answered.">
            <RichTable
              rows={data.log}
              empty="No alerts yet."
              noun="alerts"
              rowKey={(r) => String(r.id)}
              initialSort={{ id: "Time", desc: true }}
              columns={[
                { title: "Time", sort: (r) => Number(r.at_micros), cell: (r) => <span className="text-xs whitespace-nowrap">{fmtTime(r.at_micros)}</span> },
                { id: "State", title: "", sort: (r) => r.state, cell: (r) => <Tone tone={r.state === "resolved" ? "ok" : r.state === "test" ? "muted" : "bad"}>{r.state}</Tone> },
                {
                  title: "Message",
                  className: "max-w-xl whitespace-normal",
                  cell: (r) => (
                    <div className="min-w-0 space-y-1">
                      <div className="text-sm font-medium break-words">{r.title}</div>
                      {r.detail && <div className="text-xs break-words text-muted-foreground">{r.detail}</div>}
                      {Number(r.repeats) > 0 && <div className="text-xs text-muted-foreground">+{r.repeats} repeats</div>}
                    </div>
                  ),
                },
                { title: "Delivery", className: "whitespace-normal", cell: (r) => <Results json={r.results_json} /> },
              ]}
            />
          </Section>
          <Dialog open={!!editing} onOpenChange={(o) => !o && setEditing(null)}>
            <DialogContent className="sm:max-w-lg">
              {editing && (
                <ChannelForm
                  channel={editing === "new" ? null : editing}
                  hasSecret={editing !== "new" && hasSecret(editing)}
                  taken={data.channels.filter((c) => editing === "new" || c.id !== editing.id).map((c) => c.name)}
                  onClose={() => setEditing(null)}
                  onSaved={reload}
                />
              )}
            </DialogContent>
          </Dialog>
        </>
      )}
    </>
  )
}

function Results({ json }: { json: string }) {
  const results = parseJsonOr<Record<string, string>>(json, {})
  return (
    <div className="flex flex-col gap-1">
      {Object.entries(results).map(([channel, r]) => (
        <span key={channel} className="flex min-w-0 items-center gap-2 text-xs">
          {channel !== "-" && <Mono className="shrink-0">{channel}</Mono>}
          <Tone tone={r === "sent" ? "ok" : r.startsWith("retrying") || r === "sending" ? "warn" : r.startsWith("no channel") || r.startsWith("held back") ? "muted" : "bad"}>
            <span className="max-w-64 truncate">{r}</span>
          </Tone>
        </span>
      ))}
    </div>
  )
}

function ChannelList({ channels, hasSecret, onEdit, reload }: {
  channels: Channel[]
  hasSecret: (c: Channel) => boolean
  onEdit: (c: Channel) => void
  reload: () => void
}) {
  const { groups } = useApps()
  const appNames = useMemo(() => new Map((groups || []).map((g) => [g.key, g.name])), [groups])
  const test = async (c: Channel) => {
    try {
      await proc("admin_test_alert", c.id)
      toast.success(`Test message sent to ${c.name}`)
    } catch (e) {
      toast.error(errorText(e))
    }
    // Recent alerts lists the test and what the channel answered, failed or not.
    reload()
  }
  const remove = async (c: Channel) => {
    if (!confirm(`Remove the channel ${c.name} and its secret?`)) return
    if (await tryAct(() => call("admin_delete_alert_channel", c.id), "Removed") !== undefined) reload()
  }
  return (
    <DataTable
      rows={channels}
      rowKey={(c) => String(c.id || c.name)}
      empty="No channel yet: add one to get alerts."
      columns={[
        {
          title: "Channel",
          className: "whitespace-normal",
          cell: (c) => (
            <div className="min-w-0 space-y-0.5">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-medium break-all">{c.name}</span>
                {c.enabled ? <Tone tone="ok">on</Tone> : <Tone tone="muted">off</Tone>}
              </div>
              <div className="text-xs text-muted-foreground">{kindDef(c.kind).title}</div>
            </div>
          ),
        },
        {
          title: "Apps",
          className: "max-w-64 whitespace-normal",
          cell: (c) => (c.gameKeys.length
            ? <span className="text-sm break-words">{c.gameKeys.map((k) => appNames.get(k) || k).join(", ")}</span>
            : <span className="text-sm text-muted-foreground">Every app and server-wide</span>),
        },
        {
          title: "Rules",
          className: "max-w-64 whitespace-normal",
          cell: (c) => (c.rules.length
            ? <span className="text-sm break-words">{c.rules.map(ruleTitle).join(", ")}</span>
            : <span className="text-sm text-muted-foreground">Every rule</span>),
        },
        {
          title: "Secret",
          cell: (c) => (hasSecret(c)
            ? <Tone tone="ok">set</Tone>
            : <Tone tone={kindDef(c.kind).secretOptional ? "muted" : "bad"}>{kindDef(c.kind).secretOptional ? "none" : "not set"}</Tone>),
        },
        {
          title: "",
          className: "text-right",
          cell: (c) => (
            <div className="flex justify-end gap-1">
              <Button variant="outline" size="sm" onClick={() => test(c)}><SendIcon /> Test</Button>
              <Button variant="ghost" size="icon-sm" aria-label="Edit" onClick={() => onEdit(c)}><PencilIcon /></Button>
              <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Remove" onClick={() => remove(c)}><Trash2Icon /></Button>
            </div>
          ),
        },
      ]}
    />
  )
}

/** Adds (`channel` null) or edits a channel. The kind of an existing channel cannot change. */
function ChannelForm({ channel, hasSecret, taken, onClose, onSaved }: {
  channel: Channel | null
  hasSecret: boolean
  /** Names of the other channels. */
  taken: string[]
  onClose: () => void
  onSaved: () => void
}) {
  const { groups } = useApps()
  const [kind, setKind] = useState<ChannelKind>(channel?.kind || "telegram")
  const def = kindDef(kind)
  const saved = parseJsonOr<Record<string, string>>(channel?.settings_json, {})
  const [name, setName] = useState(channel?.name || "")
  const [setting, setSetting] = useState(def.setting ? saved[def.setting.key] || "" : "")
  const [secret, setSecret] = useState("")
  const [enabled, setEnabled] = useState(channel ? channel.enabled : true)
  const [gameKeys, setGameKeys] = useState<string[]>(channel?.gameKeys || [])
  const [rules, setRules] = useState<string[]>(channel?.rules || [])
  const [busy, setBusy] = useState(false)

  const pickKind = (k: ChannelKind) => {
    setKind(k)
    setSetting("")
  }
  const toggleRule = (id: string, on: boolean) => setRules((r) => (on ? [...r, id] : r.filter((x) => x !== id)))

  const save = async () => {
    const finalName = name.trim() || kind
    // The server takes a new channel with a taken name as an edit of that channel: refuse it here.
    if (taken.includes(finalName)) return toast.error(`Another channel is named ${finalName}: pick another name`)
    if (enabled && !def.secretOptional && !hasSecret && !secret.trim()) return toast.error(`Enter the ${def.secretLabel.toLowerCase()} or switch the channel off`)
    // Apps removed since the channel was saved: the server would refuse them.
    const keys = groups ? gameKeys.filter((k) => groups.some((g) => g.key === k)) : gameKeys
    const body = {
      id: channel?.id || 0,
      name: finalName,
      kind,
      enabled,
      settings: def.setting ? { [def.setting.key]: setting.trim() } : {},
      secret: secret.trim(),
      game_keys: keys,
      rules,
    }
    setBusy(true)
    const ok = await tryAct(() => call("admin_upsert_alert_channel", JSON.stringify(body)), `${finalName} saved`)
    setBusy(false)
    if (ok !== undefined) { onSaved(); onClose() }
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>{channel ? `Edit ${channel.name}` : "Add channel"}</DialogTitle>
        <DialogDescription>
          {channel ? `${def.title} channel. The kind cannot change: add a new channel for another one.` : "Where to send alerts and which ones."}
        </DialogDescription>
      </DialogHeader>
      <DialogBody>
        <div className="grid gap-4">
          <div className="grid items-start gap-4 sm:grid-cols-2">
            <Field label="Name" hint="Shown in the alert log.">
              <Input value={name} placeholder={kind} maxLength={60} onChange={(e) => setName(e.target.value)} autoFocus={!channel} />
            </Field>
            {!channel && (
              <Field label="Kind">
                <Select value={kind} onValueChange={(v) => pickKind(v as ChannelKind)}>
                  <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                  <SelectContent>{CHANNEL_KINDS.map((k) => <SelectItem key={k.kind} value={k.kind}>{k.title}</SelectItem>)}</SelectContent>
                </Select>
              </Field>
            )}
          </div>
          {def.setting && (
            <Field label={def.setting.label} hint={def.setting.hint}>
              <Input className="font-mono text-xs" placeholder={def.setting.placeholder} value={setting} onChange={(e) => setSetting(e.target.value)} />
            </Field>
          )}
          <Field label={<>{def.secretLabel} {hasSecret ? <Tone tone="ok">set</Tone> : <Tone tone="muted">not set</Tone>}</>} hint={def.secretHint}>
            <SecretField value={secret} onChange={setSecret} placeholder={hasSecret ? "Stored. Leave empty to keep it." : ""} />
          </Field>
          <div className="flex items-center gap-2">
            <Switch id="alert-channel-enabled" checked={enabled} onCheckedChange={setEnabled} />
            <Label htmlFor="alert-channel-enabled" className="text-sm">Send alerts here</Label>
          </div>
          <Field label="Apps">
            <AppsPicker
              value={gameKeys}
              onChange={setGameKeys}
              summary={gameKeys.length
                ? `Only alerts of ${gameKeys.length} app${gameKeys.length === 1 ? "" : "s"}; no server-wide alerts (exchange rates).`
                : "Every app and the server-wide alerts. Tick apps to send only theirs here."}
            />
          </Field>
          <Field label="Rules" hint={rules.length ? `Only ${rules.length} of ${RULES.length} rules.` : "Every rule. Tick rules to send only those here."}>
            <div className="space-y-1 rounded-md border p-2">
              {RULES.map((r) => (
                <label key={r.key} className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-sm hover:bg-muted">
                  <Checkbox className="shrink-0" checked={rules.includes(r.key)} onCheckedChange={(v) => toggleRule(r.key, v === true)} />
                  <span className="min-w-0 truncate">{r.title}</span>
                  <Mono className="ms-auto shrink-0 text-muted-foreground">{r.key}</Mono>
                </label>
              ))}
            </div>
          </Field>
        </div>
      </DialogBody>
      <DialogFooter>
        <Button variant="outline" onClick={onClose}>Cancel</Button>
        <Button onClick={save} disabled={busy}>Save</Button>
      </DialogFooter>
    </>
  )
}

function RulesSection({ initial, reload }: { initial: Rules; reload: () => void }) {
  const [rules, setRules] = useState<Rules>(initial)
  const set = <K extends keyof Rules>(k: K, v: Rules[K]) => setRules((r) => ({ ...r, [k]: v }))
  const num = (t: Threshold, v: string) => set(t.key, Math.max(0, Math.floor(Number(v) || 0)) as never)
  // Clamped when saved, not while typing: clearing the field to type a new number must stay possible.
  const clamped = () => {
    const out = { ...rules }
    for (const t of [WINDOW, ...RULES.flatMap((r) => (r.threshold ? [r.threshold] : []))]) {
      out[t.key] = Math.min(t.max, Math.max(t.min, Number(out[t.key]))) as never
    }
    return out
  }
  const save = async () => {
    const next = clamped()
    setRules(next)
    const ok = await tryAct(() => call("admin_set_alert_rules", JSON.stringify(next)), "Rules saved")
    if (ok !== undefined) reload()
  }
  return (
    <Section
      title="Rules"
      description="What to alert about, for every channel. Problems are per app, rejected store keys per developer account, exchange rates server-wide."
      actions={<Button onClick={save}>Save rules</Button>}
    >
      <div className="grid gap-4">
        {RULES.map((r) => (
          <div key={r.key} className="flex flex-wrap items-start justify-between gap-3 border-b pb-4 last:border-b-0 last:pb-0">
            <div className="flex min-w-0 flex-1 basis-64 items-start gap-3">
              <Switch id={`rule-${r.key}`} className="mt-0.5 shrink-0" checked={!!rules[r.key]} onCheckedChange={(v) => set(r.key, v as never)} />
              <div className="min-w-0">
                <Label htmlFor={`rule-${r.key}`} className="text-sm font-medium">{r.title}</Label>
                <p className="text-xs text-muted-foreground">{r.description}</p>
              </div>
            </div>
            {r.threshold && (
              <div className="flex shrink-0 items-center gap-2">
                <Label className="text-xs text-muted-foreground">{r.threshold.label}</Label>
                <Input
                  type="number"
                  className="w-20"
                  min={r.threshold.min}
                  max={r.threshold.max}
                  value={String(rules[r.threshold.key])}
                  onChange={(e) => num(r.threshold!, e.target.value)}
                />
              </div>
            )}
          </div>
        ))}
        <div className="flex flex-wrap items-center gap-6 pt-2">
          <div className="flex items-center gap-2">
            <Label className="text-xs text-muted-foreground">One message per problem and app every</Label>
            <Input type="number" className="w-20" min={WINDOW.min} max={WINDOW.max} value={String(rules.window_hours)} onChange={(e) => num(WINDOW, e.target.value)} />
            <span className="text-xs text-muted-foreground">hours</span>
          </div>
          <div className="flex items-center gap-2">
            <Switch id="send-resolved" checked={rules.send_resolved} onCheckedChange={(v) => set("send_resolved", v)} />
            <Label htmlFor="send-resolved" className="text-sm">Also tell when a problem is resolved</Label>
          </div>
        </div>
      </div>
    </Section>
  )
}

type OpenProblem = { key: string; rule: string; app_id: string; first_at_micros: number; last_at_micros: number; suppressed: number; detail: string }

/** Open problems of every app: searchable by app or problem, dismissable one by one or all shown at once. */
function OpenProblems({ active, reload }: { active: OpenProblem[]; reload: () => void }) {
  const { builds, groupOf } = useAppIndex()
  const [query, setQuery] = useState("")
  const appOf = (id: string) => {
    const a = builds.get(id)
    const g = groupOf.get(id)
    return a && g ? `${g.name} · ${buildLabel(a)}` : id
  }
  /** Rejected store keys are one problem per developer account (`store_auth/@{account}`), seen first on `app_id`. */
  const subjectOf = (s: OpenProblem) => {
    const subject = s.key.slice(s.key.indexOf("/") + 1)
    return subject.startsWith("@") ? `account ${subject.slice(1)} (${appOf(s.app_id)})` : appOf(s.app_id)
  }
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase()
    return q ? active.filter((s) => `${s.rule} ${s.app_id} ${subjectOf(s)} ${s.detail}`.toLowerCase().includes(q)) : active
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, query, builds, groupOf])
  const dismissAll = async () => {
    if (!confirm(`Dismiss ${shown.length} problem(s)? They are reported again if they come back.`)) return
    if (await tryAct(() => call("admin_dismiss_alerts", JSON.stringify(shown.map((s) => s.key))), "Dismissed") !== undefined) reload()
  }
  return (
    <Section title="Open problems" description="Problems reported and not resolved yet, and repeats waiting for the next message.">
      <RichTable
        rows={shown}
        rowKey={(s) => s.key}
        noun="problems"
        empty={active.length ? "No problem matches." : "Nothing open."}
        initialSort={{ id: "Last", desc: true }}
        toolbar={
          <div className="flex flex-wrap items-center gap-2">
            <Input className="h-8 w-64" placeholder="Filter by app or problem" value={query} onChange={(e) => setQuery(e.target.value)} />
            {shown.length > 1 && <Button variant="outline" size="sm" onClick={dismissAll}>Dismiss all {shown.length}</Button>}
          </div>
        }
        columns={[
          { title: "Problem", size: 200, sort: (s) => s.rule, cell: (s) => <Mono>{s.rule}</Mono> },
          { title: "App", size: 220, sort: (s) => subjectOf(s), cell: (s) => (s.app_id ? <span className="block truncate" title={s.app_id}>{subjectOf(s)}</span> : <span className="text-muted-foreground">server</span>) },
          { title: "Since", size: 150, sort: (s) => Number(s.first_at_micros), cell: (s) => <span className="text-xs whitespace-nowrap">{fmtTime(s.first_at_micros)}</span> },
          { title: "Last", size: 150, sort: (s) => Number(s.last_at_micros), cell: (s) => <span className="text-xs whitespace-nowrap">{fmtTime(s.last_at_micros)}</span> },
          { title: "Unreported", size: 100, num: true, sort: (s) => s.suppressed, cell: (s) => s.suppressed },
          { title: "Detail", size: 360, className: "whitespace-normal", cell: (s) => <span className="line-clamp-3 text-xs break-words">{s.detail}</span> },
          {
            title: "",
            size: 90,
            cell: (s) => (
              <Button variant="ghost" size="xs" onClick={async () => { if (await tryAct(() => call("admin_dismiss_alert", s.key), "Dismissed") !== undefined) reload() }}>
                Dismiss
              </Button>
            ),
          },
        ]}
      />
    </Section>
  )
}
