import { useState } from "react"
import { SendIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { DataTable, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { SecretField } from "@/components/secret-field"
import { call, getSetting, proc, select } from "@/lib/api"
import { errorText, fmtTime } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { parseJsonOr } from "@/lib/utils"

type ChannelKind = "telegram" | "slack" | "discord" | "webhook"

interface ChannelRow {
  kind: ChannelKind
  enabled: boolean
  settings_json: string
}

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

const CHANNELS: { kind: ChannelKind; title: string; secret: string; secretLabel: string; secretHint: string; setting?: { key: "chat_id" | "url"; label: string; placeholder: string; hint: string } }[] = [
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
    setting: { key: "url", label: "URL", placeholder: "https://ops.example.com/billing-alerts", hint: "Receives a JSON POST per alert." },
  },
]

interface Threshold {
  key: keyof Rules
  label: string
  min: number
  max: number
}

const WINDOW: Threshold = { key: "window_hours", label: "Hours", min: 1, max: 168 }

const RULES: { key: keyof Rules; title: string; description: string; threshold?: Threshold }[] = [
  {
    key: "delivery_failing",
    title: "Purchase delivery keeps failing",
    description: "Grant / revoke delivery to the game, or Google consume / acknowledge, failed several times in a row. Resolved when nothing is stuck any more.",
    threshold: { key: "delivery_attempts", label: "Failed attempts", min: 1, max: 50 },
  },
  { key: "store_auth", title: "Store rejected the credentials", description: "Google Play 401/403 or a rejected service account key, App Store 401/403. Resolved by the next successful verification." },
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
      channels: Object.fromEntries(channels.map((c) => [c.kind, c])) as Record<string, ChannelRow>,
      secrets: new Set(creds.map((c) => c.key)),
      rules: { ...DEFAULT_RULES, ...parsed },
      active: states.filter((s) => s.active || Number(s.suppressed) > 0).sort((a, b) => Number(b.last_at_micros) - Number(a.last_at_micros)),
      log: log.sort((a, b) => Number(b.id) - Number(a.id)).slice(0, 100),
    }
  })
  return (
    <>
      <PageHeader
        title="Alerts"
        description="Messages to Telegram, Slack, Discord or your own webhook when something needs attention, so you do not have to watch this page. At most one message per problem and app per window; repeats are counted into the next one."
      />
      {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={6} /> : (
        <>
          <div className="grid gap-6 lg:grid-cols-2">
            {CHANNELS.map((c) => (
              <ChannelCard key={c.kind} def={c} row={data.channels[c.kind]} hasSecret={data.secrets.has(`default/${c.secret}`)} reload={reload} />
            ))}
          </div>
          <RulesSection key={JSON.stringify(data.rules)} initial={data.rules} reload={reload} />
          <Section title="Open problems" description="Problems reported and not resolved yet, and repeats waiting for the next message.">
            <DataTable
              rows={data.active}
              empty="Nothing open."
              columns={[
                { title: "Problem", cell: (s) => <span className="flex flex-wrap items-center gap-2"><Mono>{s.rule}</Mono>{s.app_id && <Mono className="text-muted-foreground">{s.app_id}</Mono>}</span> },
                { title: "Since", cell: (s) => <span className="text-xs whitespace-nowrap">{fmtTime(s.first_at_micros)}</span> },
                { title: "Last", cell: (s) => <span className="text-xs whitespace-nowrap">{fmtTime(s.last_at_micros)}</span> },
                { title: "Unreported", num: true, cell: (s) => s.suppressed },
                { title: "Detail", className: "max-w-md whitespace-normal", cell: (s) => <span className="text-xs break-words">{s.detail}</span> },
                {
                  title: "",
                  className: "text-right",
                  cell: (s) => (
                    <Button variant="ghost" size="xs" onClick={async () => { if (await tryAct(() => call("admin_dismiss_alert", s.key), "Dismissed") !== undefined) reload() }}>
                      Dismiss
                    </Button>
                  ),
                },
              ]}
            />
          </Section>
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
          <Tone tone={r === "sent" ? "ok" : r.startsWith("retrying") || r === "sending" ? "warn" : r.startsWith("no channel") ? "muted" : "bad"}>
            <span className="max-w-64 truncate">{r}</span>
          </Tone>
        </span>
      ))}
    </div>
  )
}

function ChannelCard({ def, row, hasSecret, reload }: { def: (typeof CHANNELS)[number]; row?: ChannelRow; hasSecret: boolean; reload: () => void }) {
  const saved = parseJsonOr<Record<string, string>>(row?.settings_json, {})
  const [enabled, setEnabled] = useState(row ? !!row.enabled : true)
  const [setting, setSetting] = useState(def.setting ? saved[def.setting.key] || "" : "")
  const [secret, setSecret] = useState("")

  const save = async () => {
    const settings = def.setting ? { [def.setting.key]: setting.trim() } : {}
    const ok = await tryAct(
      () => call("admin_upsert_alert_channel", JSON.stringify({ kind: def.kind, enabled, settings, secret: secret.trim() })),
      `${def.title} saved`,
    )
    if (ok !== undefined) { setSecret(""); reload() }
  }
  const test = async () => {
    try {
      await proc("admin_test_alert", def.kind)
      toast.success(`Test message sent to ${def.title}`)
    } catch (e) {
      toast.error(errorText(e))
    }
    // Recent alerts lists the test and what the channel answered, failed or not.
    reload()
  }
  const remove = async () => {
    if (!confirm(`Remove the ${def.title} channel and its secret?`)) return
    if (await tryAct(() => call("admin_delete_alert_channel", def.kind), "Removed") !== undefined) reload()
  }

  return (
    <Section
      title={def.title}
      description={row ? (row.enabled ? <Tone tone="ok">on</Tone> : <Tone tone="muted">off</Tone>) : "Not configured"}
      actions={row && (
        <>
          <Button variant="outline" size="sm" onClick={test}><SendIcon /> Send test</Button>
          <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Remove" onClick={remove}><Trash2Icon /></Button>
        </>
      )}
    >
      <div className="grid gap-4">
        {def.setting && (
          <Field label={def.setting.label} hint={def.setting.hint}>
            <Input className="font-mono text-xs" placeholder={def.setting.placeholder} value={setting} onChange={(e) => setSetting(e.target.value)} />
          </Field>
        )}
        <Field label={<>{def.secretLabel} {hasSecret ? <Tone tone="ok">set</Tone> : <Tone tone="muted">not set</Tone>}</>} hint={def.secretHint}>
          <SecretField value={secret} onChange={setSecret} placeholder={hasSecret ? "Stored. Type to replace it." : ""} />
        </Field>
        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <Switch id={`alert-${def.kind}`} checked={enabled} onCheckedChange={setEnabled} />
            <Label htmlFor={`alert-${def.kind}`} className="text-sm">Send alerts here</Label>
          </div>
          <Button onClick={save}>Save</Button>
        </div>
      </div>
    </Section>
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
      description="What to alert about. Store notification and credential checks are per app; exchange rates are server-wide."
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
